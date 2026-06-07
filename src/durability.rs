//! Crash-safe durability via a write-ahead log.
//!
//! Without durability, a write is acknowledged once it is in the in-memory
//! memtable; a crash before the next flush loses it. The `durability` feature
//! closes that gap: every write is appended to a `wal-db` log and made durable
//! *before* it is acknowledged, and on open the log is replayed into the memtable
//! so no acknowledged write is lost across a crash.
//!
//! The log holds only the writes since the last flush. A flush makes those writes
//! durable in a sorted run, so the log is rotated — emptied — at that point;
//! recovery only ever replays the small tail of writes that had not yet been
//! flushed.
//!
//! This module presents one type, [`Durability`], with the same surface either
//! way: with the feature off it is a zero-sized no-op, so the engine calls it
//! unconditionally and pays nothing on the non-durable path.

#[cfg(feature = "durability")]
pub(crate) use enabled::Durability;

#[cfg(not(feature = "durability"))]
pub(crate) use disabled::Durability;

#[cfg(not(feature = "durability"))]
mod disabled {
    use std::path::Path;

    use crate::error::Result;
    use crate::memtable::MemTable;
    use crate::record::Record;

    /// No-op durability used when the `durability` feature is disabled.
    #[derive(Debug)]
    pub(crate) struct Durability;

    impl Durability {
        #[inline]
        pub(crate) fn open(_dir: &Path) -> Result<Self> {
            Ok(Durability)
        }

        #[inline]
        pub(crate) fn log_one(&self, _key: &[u8], _record: &Record) -> Result<()> {
            Ok(())
        }

        #[inline]
        pub(crate) fn log_batch(&self, _ops: &[(Vec<u8>, Record)]) -> Result<()> {
            Ok(())
        }

        #[inline]
        pub(crate) fn rotate(&mut self) -> Result<()> {
            Ok(())
        }

        #[inline]
        pub(crate) fn replay(&self, _mem: &mut MemTable) -> Result<()> {
            Ok(())
        }
    }
}

#[cfg(feature = "durability")]
mod enabled {
    use std::iter;
    use std::path::{Path, PathBuf};

    use wal_db::Wal;

    use crate::error::{Error, Result};
    use crate::memtable::MemTable;
    use crate::record::Record;

    /// Name of the write-ahead log file inside the database directory.
    const WAL_FILE: &str = "wal.log";

    /// Tag byte for a buffered value in a log record.
    const TAG_VALUE: u8 = 0;
    /// Tag byte for a tombstone in a log record.
    const TAG_TOMBSTONE: u8 = 1;

    /// Upper bound on a length prefix decoded from the log, guarding replay
    /// against a corrupt count driving an unbounded allocation.
    const MAX_LEN: u32 = 1 << 30;

    /// A `wal-db`-backed write-ahead log.
    ///
    /// All methods are called under the engine's write lock, so the log is never
    /// touched concurrently — which is also what `truncate`/rotate require.
    #[derive(Debug)]
    pub(crate) struct Durability {
        /// `Some` except for the brief moment inside [`rotate`](Self::rotate).
        wal: Option<Wal>,
        path: PathBuf,
    }

    impl Durability {
        /// Open (or create) the log in `dir`.
        pub(crate) fn open(dir: &Path) -> Result<Self> {
            let path = dir.join(WAL_FILE);
            let wal = Wal::open(&path).map_err(|e| wal_err("open write-ahead log", e))?;
            Ok(Durability {
                wal: Some(wal),
                path,
            })
        }

        /// Durably log a single operation before it is acknowledged.
        pub(crate) fn log_one(&self, key: &[u8], record: &Record) -> Result<()> {
            let bytes = encode(iter::once((key, record)));
            self.append(&bytes)
        }

        /// Durably log a group of operations as one atomic record.
        pub(crate) fn log_batch(&self, ops: &[(Vec<u8>, Record)]) -> Result<()> {
            let bytes = encode(ops.iter().map(|(k, r)| (k.as_slice(), r)));
            self.append(&bytes)
        }

        fn append(&self, bytes: &[u8]) -> Result<()> {
            let wal = self.wal()?;
            let _ = wal
                .append_and_sync(bytes)
                .map_err(|e| wal_err("append to write-ahead log", e))?;
            Ok(())
        }

        /// Empty the log after a flush has made its contents durable in a run.
        ///
        /// A single-file `wal-db` log cannot reclaim a prefix in place, so the
        /// log is recreated: the handle is dropped, the file removed, and a fresh
        /// empty log opened. Safe because callers hold the engine write lock, so
        /// nothing else touches the log.
        pub(crate) fn rotate(&mut self) -> Result<()> {
            self.wal = None; // close the handle before removing the file (Windows)
            match std::fs::remove_file(&self.path) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(Error::io("rotate write-ahead log", e)),
            }
            let wal = Wal::open(&self.path).map_err(|e| wal_err("reopen write-ahead log", e))?;
            self.wal = Some(wal);
            Ok(())
        }

        /// Replay the log into `mem`, applying every record in append order so
        /// the latest write per key wins.
        pub(crate) fn replay(&self, mem: &mut MemTable) -> Result<()> {
            let wal = self.wal()?;
            for entry in wal.iter().map_err(|e| wal_err("read write-ahead log", e))? {
                let entry = entry.map_err(|e| wal_err("read log record", e))?;
                for (key, record) in decode(entry.data())? {
                    mem.apply(key, record);
                }
            }
            Ok(())
        }

        fn wal(&self) -> Result<&Wal> {
            self.wal
                .as_ref()
                .ok_or_else(|| Error::corruption("write-ahead log not open"))
        }
    }

    /// Encode a sequence of operations into one log record.
    fn encode<'a>(ops: impl ExactSizeIterator<Item = (&'a [u8], &'a Record)>) -> Vec<u8> {
        let mut buf = Vec::with_capacity(8 + ops.len() * 16);
        buf.extend_from_slice(&(ops.len() as u32).to_le_bytes());
        for (key, record) in ops {
            buf.extend_from_slice(&(key.len() as u32).to_le_bytes());
            buf.extend_from_slice(key);
            match record {
                Record::Value(value) => {
                    buf.push(TAG_VALUE);
                    buf.extend_from_slice(&(value.len() as u32).to_le_bytes());
                    buf.extend_from_slice(value);
                }
                Record::Tombstone => {
                    buf.push(TAG_TOMBSTONE);
                    buf.extend_from_slice(&0u32.to_le_bytes());
                }
            }
        }
        buf
    }

    /// Decode one log record into its operations, bounds-checking every field.
    fn decode(bytes: &[u8]) -> Result<Vec<(Vec<u8>, Record)>> {
        let mut pos = 0usize;
        let count = read_u32(bytes, &mut pos)?;
        let mut ops = Vec::with_capacity(count.min(1024) as usize);
        for _ in 0..count {
            let key_len = read_u32(bytes, &mut pos)?;
            if key_len > MAX_LEN {
                return Err(Error::corruption("log key length exceeds maximum"));
            }
            let key = read_bytes(bytes, &mut pos, key_len as usize)?;
            let tag = read_u8(bytes, &mut pos)?;
            let value_len = read_u32(bytes, &mut pos)?;
            if value_len > MAX_LEN {
                return Err(Error::corruption("log value length exceeds maximum"));
            }
            let record = match tag {
                TAG_VALUE => Record::Value(read_bytes(bytes, &mut pos, value_len as usize)?),
                TAG_TOMBSTONE => {
                    if value_len != 0 {
                        return Err(Error::corruption(
                            "log tombstone with non-zero value length",
                        ));
                    }
                    Record::Tombstone
                }
                _ => return Err(Error::corruption("unknown log record tag")),
            };
            ops.push((key, record));
        }
        if pos != bytes.len() {
            return Err(Error::corruption("trailing bytes in log record"));
        }
        Ok(ops)
    }

    fn read_u8(bytes: &[u8], pos: &mut usize) -> Result<u8> {
        let b = *bytes
            .get(*pos)
            .ok_or_else(|| Error::corruption("log record truncated"))?;
        *pos += 1;
        Ok(b)
    }

    fn read_u32(bytes: &[u8], pos: &mut usize) -> Result<u32> {
        let end = pos
            .checked_add(4)
            .ok_or_else(|| Error::corruption("log record overflow"))?;
        let slice = bytes
            .get(*pos..end)
            .ok_or_else(|| Error::corruption("log record truncated"))?;
        let mut arr = [0u8; 4];
        arr.copy_from_slice(slice);
        *pos = end;
        Ok(u32::from_le_bytes(arr))
    }

    fn read_bytes(bytes: &[u8], pos: &mut usize, len: usize) -> Result<Vec<u8>> {
        let end = pos
            .checked_add(len)
            .ok_or_else(|| Error::corruption("log record overflow"))?;
        let slice = bytes
            .get(*pos..end)
            .ok_or_else(|| Error::corruption("log record truncated"))?;
        *pos = end;
        Ok(slice.to_vec())
    }

    /// Wrap a `wal-db` error as the crate's I/O error, preserving its message.
    fn wal_err(context: &'static str, e: wal_db::WalError) -> Error {
        Error::io(context, std::io::Error::other(e.to_string()))
    }

    #[cfg(test)]
    #[allow(clippy::unwrap_used, clippy::expect_used)]
    mod tests {
        use super::*;

        #[test]
        fn test_encode_decode_roundtrip() {
            let ops = vec![
                (b"a".to_vec(), Record::Value(b"1".to_vec())),
                (b"b".to_vec(), Record::Tombstone),
                (b"c".to_vec(), Record::Value(Vec::new())),
            ];
            let bytes = encode(ops.iter().map(|(k, r)| (k.as_slice(), r)));
            assert_eq!(decode(&bytes).unwrap(), ops);
        }

        #[test]
        fn test_decode_rejects_trailing_bytes() {
            let mut bytes = encode(iter::once((b"k".as_slice(), &Record::Value(b"v".to_vec()))));
            bytes.push(0xFF);
            assert!(decode(&bytes).is_err());
        }

        #[test]
        fn test_decode_rejects_truncation() {
            let bytes = encode(iter::once((b"k".as_slice(), &Record::Value(b"v".to_vec()))));
            assert!(decode(&bytes[..bytes.len() - 1]).is_err());
        }

        #[test]
        fn test_log_replay_roundtrip() {
            let dir = tempfile::tempdir().unwrap();
            {
                let d = Durability::open(dir.path()).unwrap();
                d.log_one(b"a", &Record::Value(b"1".to_vec())).unwrap();
                d.log_one(b"a", &Record::Value(b"2".to_vec())).unwrap();
                d.log_one(b"b", &Record::Tombstone).unwrap();
                d.log_batch(&[
                    (b"c".to_vec(), Record::Value(b"3".to_vec())),
                    (b"d".to_vec(), Record::Value(b"4".to_vec())),
                ])
                .unwrap();
            }
            let d = Durability::open(dir.path()).unwrap();
            let mut mem = MemTable::new();
            d.replay(&mut mem).unwrap();
            assert_eq!(mem.get(b"a"), Some(&Record::Value(b"2".to_vec()))); // latest wins
            assert_eq!(mem.get(b"b"), Some(&Record::Tombstone));
            assert_eq!(mem.get(b"c"), Some(&Record::Value(b"3".to_vec())));
            assert_eq!(mem.get(b"d"), Some(&Record::Value(b"4".to_vec())));
        }

        #[test]
        fn test_rotate_empties_log() {
            let dir = tempfile::tempdir().unwrap();
            let mut d = Durability::open(dir.path()).unwrap();
            d.log_one(b"a", &Record::Value(b"1".to_vec())).unwrap();
            d.rotate().unwrap();

            let mut mem = MemTable::new();
            d.replay(&mut mem).unwrap();
            assert!(mem.is_empty());

            // Reopening after rotation also sees an empty log.
            let d2 = Durability::open(dir.path()).unwrap();
            let mut mem2 = MemTable::new();
            d2.replay(&mut mem2).unwrap();
            assert!(mem2.is_empty());
        }
    }
}
