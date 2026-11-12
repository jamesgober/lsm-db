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
//! unconditionally and pays nothing on the non-durable path. The one thing it
//! still does then is refuse a directory whose log holds writes: a build
//! without the feature cannot replay them, and a later build with it would
//! replay them over the newer data written in between.
//!
//! With the feature on, the log is opened with wal-db's strict policy: a
//! damaged final record (a write torn by a crash, never acknowledged) is cut,
//! but damage followed by intact records refuses the open with
//! [`Error::Corruption`] and leaves the log as it is, since cutting there
//! would drop acknowledged writes.

#[cfg(feature = "durability")]
pub(crate) use enabled::Durability;

#[cfg(not(feature = "durability"))]
pub(crate) use disabled::Durability;

/// Name of the write-ahead log file inside the database directory.
const WAL_FILE: &str = "wal.log";

#[cfg(not(feature = "durability"))]
mod disabled {
    use std::path::Path;

    use super::WAL_FILE;
    use crate::error::{Error, Result};
    use crate::memtable::MemTable;
    use crate::record::Record;

    /// No-op durability used when the `durability` feature is disabled.
    #[derive(Debug)]
    pub(crate) struct Durability;

    impl Durability {
        /// Refuse a directory whose write-ahead log is not empty. A build
        /// with `durability` empties the log at every flush and leaves an
        /// empty file, so a non-empty one holds writes that only such a
        /// build can replay.
        pub(crate) fn check(dir: &Path) -> Result<()> {
            match std::fs::metadata(dir.join(WAL_FILE)) {
                Ok(meta) if meta.len() > 0 => Err(Error::invalid_input(
                    "wal.log holds writes that a build without the durability feature cannot \
                     replay; open the directory with the durability feature first",
                )),
                Ok(_) => Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(e) => Err(Error::io("stat write-ahead log", e)),
            }
        }

        #[inline]
        pub(crate) fn open(_dir: &Path) -> Result<Self> {
            Ok(Durability)
        }

        #[inline]
        pub(crate) fn log_one(&mut self, _key: &[u8], _record: &Record) -> Result<()> {
            Ok(())
        }

        #[inline]
        pub(crate) fn log_batch(&mut self, _ops: &[(Vec<u8>, Record)]) -> Result<()> {
            Ok(())
        }

        #[inline]
        pub(crate) fn rotate(&mut self) -> Result<()> {
            Ok(())
        }

        #[inline]
        pub(crate) fn replay(&mut self, _mem: &mut MemTable) -> Result<()> {
            Ok(())
        }
    }
}

#[cfg(feature = "durability")]
mod enabled {
    use std::iter;
    use std::path::{Path, PathBuf};

    use wal_db::{OpenPolicy, Wal, WalConfig, WalError};

    use super::WAL_FILE;
    use crate::error::{Error, Result};
    use crate::format::{MAX_KEY_LEN, MAX_VALUE_LEN};
    use crate::memtable::MemTable;
    use crate::record::Record;

    /// Framing of a single write in a log record: the operation count, the
    /// key length, the tag, and the value length.
    const ONE_WRITE_FRAMING: usize = 4 + 4 + 1 + 4;

    /// The largest log record: one write of the longest key and value. A
    /// batch whose record is larger is refused with `InvalidInput` before
    /// anything is logged or applied. (wal-db's own default is 64 MiB.)
    const MAX_RECORD: usize = MAX_KEY_LEN + MAX_VALUE_LEN + ONE_WRITE_FRAMING;

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
        /// `None` only after a rotation removed the old log and could not
        /// open a new one; the next use opens it again.
        wal: Option<Wal>,
        path: PathBuf,
    }

    /// Open the log at `path` with the engine's settings.
    fn open_wal(path: &Path, context: &'static str) -> Result<Wal> {
        let config = WalConfig::new()
            .with_open_policy(OpenPolicy::Strict)
            .with_max_record_size(MAX_RECORD as u32);
        Wal::open_with(path, config).map_err(|e| wal_err(context, e))
    }

    impl Durability {
        /// Nothing to check: this build replays any log it finds.
        #[inline]
        pub(crate) fn check(_dir: &Path) -> Result<()> {
            Ok(())
        }

        /// Open (or create) the log in `dir`.
        pub(crate) fn open(dir: &Path) -> Result<Self> {
            let path = dir.join(WAL_FILE);
            let wal = open_wal(&path, "open write-ahead log")?;
            Ok(Durability {
                wal: Some(wal),
                path,
            })
        }

        /// Durably log a single operation before it is acknowledged.
        pub(crate) fn log_one(&mut self, key: &[u8], record: &Record) -> Result<()> {
            let bytes = encode(iter::once((key, record)));
            self.append(&bytes)
        }

        /// Durably log a group of operations as one atomic record.
        ///
        /// # Errors
        ///
        /// [`Error::InvalidInput`] when the batch's record would pass the
        /// log's record limit; nothing is logged.
        pub(crate) fn log_batch(&mut self, ops: &[(Vec<u8>, Record)]) -> Result<()> {
            check_record_len(encoded_len(ops.iter().map(|(k, r)| (k.as_slice(), r))))?;
            let bytes = encode(ops.iter().map(|(k, r)| (k.as_slice(), r)));
            self.append(&bytes)
        }

        fn append(&mut self, bytes: &[u8]) -> Result<()> {
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
        ///
        /// When the file cannot be removed (on Windows, another program may
        /// hold it open) the old log is opened again and kept: its records
        /// are already in runs, and replaying them at the next open changes
        /// nothing, because every later write to the same keys follows them
        /// in the log. The next flush tries the removal again. When the new
        /// log cannot be opened, the error is returned and the next write
        /// tries to open it again.
        pub(crate) fn rotate(&mut self) -> Result<()> {
            self.wal = None; // close the handle before removing the file (Windows)
            if crate::fault::check("remove write-ahead log").is_ok() {
                let _ = std::fs::remove_file(&self.path);
            }
            crate::fault::check("reopen write-ahead log")?;
            self.wal = Some(open_wal(&self.path, "reopen write-ahead log")?);
            Ok(())
        }

        /// Replay the log into `mem`, applying every record in append order so
        /// the latest write per key wins.
        pub(crate) fn replay(&mut self, mem: &mut MemTable) -> Result<()> {
            let wal = self.wal()?;
            for entry in wal.iter().map_err(|e| wal_err("read write-ahead log", e))? {
                let entry = entry.map_err(|e| wal_err("read log record", e))?;
                for (key, record) in decode(entry.data())? {
                    mem.apply(key, record);
                }
            }
            Ok(())
        }

        /// The open log, opening it again after a rotation could not.
        fn wal(&mut self) -> Result<&Wal> {
            if self.wal.is_none() {
                self.wal = Some(open_wal(&self.path, "reopen write-ahead log")?);
            }
            self.wal
                .as_ref()
                .ok_or_else(|| Error::io("open write-ahead log", std::io::Error::other("not open")))
        }
    }

    /// The length of the record [`encode`] writes for `ops`, saturating.
    fn encoded_len<'a>(ops: impl Iterator<Item = (&'a [u8], &'a Record)>) -> usize {
        ops.fold(4usize, |len, (key, record)| {
            len.saturating_add(4 + 1 + 4)
                .saturating_add(key.len())
                .saturating_add(record.value_len())
        })
    }

    /// Refuse a record longer than the log accepts. A single write never is:
    /// its key and value are within the format's limits.
    fn check_record_len(len: usize) -> Result<()> {
        if len > MAX_RECORD {
            return Err(Error::invalid_input(
                "batch larger than the write-ahead log's record limit (2 GiB)",
            ));
        }
        Ok(())
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

    /// Map a `wal-db` error. Damage the log cannot cut without losing
    /// acknowledged writes is corruption; anything else is the crate's I/O
    /// error, keeping wal-db's message.
    fn wal_err(context: &'static str, e: WalError) -> Error {
        match e {
            WalError::Corruption { .. } => Error::corruption(
                "write-ahead log damaged before its end, with intact records after the damage",
            ),
            other => Error::io(context, std::io::Error::other(other.to_string())),
        }
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
                let mut d = Durability::open(dir.path()).unwrap();
                d.log_one(b"a", &Record::Value(b"1".to_vec())).unwrap();
                d.log_one(b"a", &Record::Value(b"2".to_vec())).unwrap();
                d.log_one(b"b", &Record::Tombstone).unwrap();
                d.log_batch(&[
                    (b"c".to_vec(), Record::Value(b"3".to_vec())),
                    (b"d".to_vec(), Record::Value(b"4".to_vec())),
                ])
                .unwrap();
            }
            let mut d = Durability::open(dir.path()).unwrap();
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
            let mut d2 = Durability::open(dir.path()).unwrap();
            let mut mem2 = MemTable::new();
            d2.replay(&mut mem2).unwrap();
            assert!(mem2.is_empty());
        }

        /// Damage in the middle of the log, with intact records after it,
        /// refuses the open and leaves the log as it was; a torn final
        /// record is still cut.
        #[test]
        fn test_mid_log_damage_is_corruption_and_a_torn_tail_is_cut() {
            let dir = tempfile::tempdir().unwrap();
            {
                let mut d = Durability::open(dir.path()).unwrap();
                for k in [b"k0", b"k1", b"k2"] {
                    d.log_one(k, &Record::Value(vec![b'v'; 64])).unwrap();
                }
            }
            let path = dir.path().join(WAL_FILE);
            let good = std::fs::read(&path).unwrap();
            let mut bytes = good.clone();
            let middle = bytes.len() / 2;
            bytes[middle] ^= 0xFF;
            std::fs::write(&path, &bytes).unwrap();
            assert!(matches!(
                Durability::open(dir.path()),
                Err(Error::Corruption { .. })
            ));
            assert_eq!(std::fs::read(&path).unwrap(), bytes, "the log is untouched");

            // A torn tail: the last record cut short.
            std::fs::write(&path, &good[..good.len() - 3]).unwrap();
            let mut d = Durability::open(dir.path()).unwrap();
            let mut mem = MemTable::new();
            d.replay(&mut mem).unwrap();
            assert_eq!(mem.len(), 2);
        }

        /// A log that cannot be removed at a rotation is opened again and
        /// kept, so writes go on; a log that cannot be reopened fails the
        /// rotation, and the next write opens it. Before 1.4 either left the
        /// handle without a log, and every later write failed with a
        /// corruption error.
        #[test]
        fn test_a_failed_rotation_does_not_wedge_the_log() {
            let dir = tempfile::tempdir().unwrap();
            let mut d = Durability::open(dir.path()).unwrap();
            d.log_one(b"a", &Record::Value(b"1".to_vec())).unwrap();

            crate::fault::arm("remove write-ahead log");
            d.rotate().unwrap();
            d.log_one(b"b", &Record::Value(b"2".to_vec())).unwrap();
            let mut mem = MemTable::new();
            d.replay(&mut mem).unwrap();
            assert_eq!(mem.len(), 2, "the kept log holds both writes");

            crate::fault::arm("reopen write-ahead log");
            assert!(matches!(d.rotate(), Err(Error::Io { .. })));
            d.log_one(b"c", &Record::Value(b"3".to_vec())).unwrap();
            let mut mem = MemTable::new();
            d.replay(&mut mem).unwrap();
            assert_eq!(mem.get(b"c"), Some(&Record::Value(b"3".to_vec())));
        }

        /// The record limit covers one write of the longest key and value,
        /// and the length a batch is checked by is the length it encodes to.
        #[test]
        fn test_record_limit_covers_the_largest_write() {
            assert!(check_record_len(MAX_KEY_LEN + MAX_VALUE_LEN + ONE_WRITE_FRAMING).is_ok());
            assert!(matches!(
                check_record_len(MAX_RECORD + 1),
                Err(Error::InvalidInput { .. })
            ));
            assert!(u32::try_from(MAX_RECORD).is_ok());
            let ops = [
                (b"a".to_vec(), Record::Value(b"12345".to_vec())),
                (b"bc".to_vec(), Record::Tombstone),
            ];
            let len = encoded_len(ops.iter().map(|(k, r)| (k.as_slice(), r)));
            assert_eq!(
                len,
                encode(ops.iter().map(|(k, r)| (k.as_slice(), r))).len()
            );
        }

        /// A write over wal-db's default 64 MiB record limit is logged and
        /// replayed (the limit is raised to the format's), not refused with
        /// an I/O error.
        #[test]
        fn test_a_write_over_64_mib_is_logged() {
            let dir = tempfile::tempdir().unwrap();
            let value = vec![7u8; 65 << 20];
            {
                let mut d = Durability::open(dir.path()).unwrap();
                d.log_one(b"big", &Record::Value(value.clone())).unwrap();
            }
            let mut d = Durability::open(dir.path()).unwrap();
            let mut mem = MemTable::new();
            d.replay(&mut mem).unwrap();
            assert_eq!(mem.get(b"big"), Some(&Record::Value(value)));
        }
    }
}
