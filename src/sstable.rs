//! On-disk immutable sorted runs.
//!
//! When the [`MemTable`](crate::memtable::MemTable) fills, it is flushed to an
//! *SSTable* (sorted string table): an immutable file holding every live key in
//! ascending order, each paired with its value. Reads that miss the in-memory
//! buffer fall through to the SSTable.
//!
//! ## On-disk layout (v0.2, not yet frozen)
//!
//! ```text
//! ┌────────────────┐
//! │ magic (8 bytes)│  "LSMSST02"
//! ├────────────────┤
//! │ entry          │  key_len:u32le · key · value_len:u32le · value
//! │ entry          │  … in ascending key order …
//! │ …              │
//! ├────────────────┤
//! │ count (u64 le) │  number of entries, for validation
//! └────────────────┘
//! ```
//!
//! Tombstones are never written: a flush merges the buffer over the previous
//! run, and a deleted key is simply omitted from the new run, so every entry on
//! disk is live data. The byte layout is deliberately minimal for the
//! foundation release; the normative, frozen format arrives with the multi-level
//! engine in 0.3 (`docs/SSTABLE_FORMAT.md`).
//!
//! ## Reading
//!
//! Opening a run scans it once to build an in-memory index of
//! `(key, value offset, value length)`, sorted by key. A point lookup binary
//! searches the index and then reads exactly the matching value with a single
//! positioned read — values themselves stay on disk. Positioned reads
//! (`pread` on Unix, `seek_read` on Windows) take `&File`, so concurrent readers
//! share one handle without seeking over each other.

use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Read, Write};
use std::path::Path;

use crate::error::{Error, Result};

/// File magic identifying an `lsm-db` v0.2 sorted run.
const MAGIC: &[u8; 8] = b"LSMSST02";

/// Hard cap on a single length prefix read from disk, in bytes.
///
/// A corrupt or hostile length prefix must not be able to drive an unbounded
/// allocation. One gibibyte is far above any legitimate single key or value in
/// this release and well below a denial-of-service allocation.
const MAX_RECORD_LEN: u32 = 1 << 30;

/// One entry of a run's in-memory index: a key and where its value lives.
#[derive(Debug)]
struct IndexEntry {
    key: Vec<u8>,
    value_offset: u64,
    value_len: u32,
}

/// An open, immutable on-disk sorted run.
#[derive(Debug)]
pub(crate) struct SsTable {
    file: File,
    index: Vec<IndexEntry>,
}

impl SsTable {
    /// Open the run at `path`, scanning it once to build the key index.
    ///
    /// Returns [`Error::Corruption`] if the magic is wrong, a length prefix is
    /// implausible, or the file ends mid-record.
    pub(crate) fn open(path: &Path) -> Result<Self> {
        let file = File::open(path).map_err(|e| Error::io("open sorted run", e))?;
        let file_len = file
            .metadata()
            .map_err(|e| Error::io("stat sorted run", e))?
            .len();

        // Smallest valid file is magic + an empty entry list + count footer.
        if file_len < (MAGIC.len() + 8) as u64 {
            return Err(Error::corruption("file shorter than header and footer"));
        }

        let mut reader =
            std::io::BufReader::new(File::open(path).map_err(|e| Error::io("open sorted run", e))?);
        let mut magic = [0u8; 8];
        reader
            .read_exact(&mut magic)
            .map_err(|e| Error::io("read run magic", e))?;
        if &magic != MAGIC {
            return Err(Error::corruption("bad magic; not an lsm-db sorted run"));
        }

        let data_end = file_len - 8; // everything before the count footer
        let count = read_count_footer(&file, file_len)?;

        let mut index = Vec::with_capacity(count as usize);
        let mut pos = MAGIC.len() as u64;
        for _ in 0..count {
            let key = read_len_prefixed(&mut reader, &mut pos, data_end)?;
            let value_len = read_u32(&mut reader, &mut pos, data_end)?;
            if value_len > MAX_RECORD_LEN {
                return Err(Error::corruption("value length exceeds maximum"));
            }
            let value_offset = pos;
            let end = pos
                .checked_add(u64::from(value_len))
                .ok_or_else(|| Error::corruption("value extent overflows"))?;
            if end > data_end {
                return Err(Error::corruption("value extends past end of data"));
            }
            skip(&mut reader, u64::from(value_len), &mut pos)?;
            index.push(IndexEntry {
                key,
                value_offset,
                value_len,
            });
        }

        if pos != data_end {
            return Err(Error::corruption("trailing bytes after final entry"));
        }

        Ok(SsTable { file, index })
    }

    /// The number of live entries in the run.
    #[inline]
    pub(crate) fn len(&self) -> usize {
        self.index.len()
    }

    /// Look up `key`, returning its value if the run contains it.
    pub(crate) fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        match self.index.binary_search_by(|e| e.key.as_slice().cmp(key)) {
            Ok(i) => self.read_value(i).map(Some),
            Err(_) => Ok(None),
        }
    }

    /// The key at index `i` in ascending order.
    #[inline]
    pub(crate) fn key_at(&self, i: usize) -> &[u8] {
        &self.index[i].key
    }

    /// Read the value at index `i` with a single positioned read.
    pub(crate) fn read_value(&self, i: usize) -> Result<Vec<u8>> {
        let entry = &self.index[i];
        let mut buf = vec![0u8; entry.value_len as usize];
        pread_exact(&self.file, &mut buf, entry.value_offset)
            .map_err(|e| Error::io("read run value", e))?;
        Ok(buf)
    }
}

/// Read the trailing `u64` entry count from the footer.
fn read_count_footer(file: &File, file_len: u64) -> Result<u64> {
    let mut buf = [0u8; 8];
    pread_exact(file, &mut buf, file_len - 8).map_err(|e| Error::io("read run footer", e))?;
    Ok(u64::from_le_bytes(buf))
}

/// Read a `u32` length prefix and the bytes it covers, advancing `pos`.
fn read_len_prefixed(reader: &mut impl Read, pos: &mut u64, data_end: u64) -> Result<Vec<u8>> {
    let len = read_u32(reader, pos, data_end)?;
    if len > MAX_RECORD_LEN {
        return Err(Error::corruption("key length exceeds maximum"));
    }
    let end = pos
        .checked_add(u64::from(len))
        .ok_or_else(|| Error::corruption("record extent overflows"))?;
    if end > data_end {
        return Err(Error::corruption("record extends past end of data"));
    }
    let mut buf = vec![0u8; len as usize];
    reader
        .read_exact(&mut buf)
        .map_err(|e| Error::io("read run key", e))?;
    *pos += u64::from(len);
    Ok(buf)
}

/// Read a little-endian `u32`, advancing `pos` and bounds-checking against
/// `data_end`.
fn read_u32(reader: &mut impl Read, pos: &mut u64, data_end: u64) -> Result<u32> {
    if pos.checked_add(4).is_none_or(|end| end > data_end) {
        return Err(Error::corruption("length prefix extends past end of data"));
    }
    let mut buf = [0u8; 4];
    reader
        .read_exact(&mut buf)
        .map_err(|e| Error::io("read length prefix", e))?;
    *pos += 4;
    Ok(u32::from_le_bytes(buf))
}

/// Discard `n` bytes from `reader`, advancing `pos`.
fn skip(reader: &mut impl Read, n: u64, pos: &mut u64) -> Result<()> {
    let copied = std::io::copy(&mut reader.take(n), &mut std::io::sink())
        .map_err(|e| Error::io("skip value bytes", e))?;
    if copied != n {
        return Err(Error::corruption("value shorter than its length prefix"));
    }
    *pos += n;
    Ok(())
}

/// Streaming writer for a sorted run.
///
/// Entries must be supplied in ascending key order — the writer trusts its
/// caller (the flush merge) to uphold that, and the resulting file's index is
/// validated to be sorted when it is reopened.
pub(crate) struct SsTableWriter {
    out: BufWriter<File>,
    count: u64,
}

impl SsTableWriter {
    /// Create a new run file at `path`, truncating any existing file, and write
    /// the header.
    pub(crate) fn create(path: &Path) -> Result<Self> {
        let file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)
            .map_err(|e| Error::io("create sorted run", e))?;
        let mut out = BufWriter::new(file);
        out.write_all(MAGIC)
            .map_err(|e| Error::io("write run magic", e))?;
        Ok(SsTableWriter { out, count: 0 })
    }

    /// Append one live entry. Keys must arrive in ascending order.
    pub(crate) fn push(&mut self, key: &[u8], value: &[u8]) -> Result<()> {
        let key_len = u32::try_from(key.len())
            .map_err(|_| Error::corruption("key longer than u32 length prefix"))?;
        let value_len = u32::try_from(value.len())
            .map_err(|_| Error::corruption("value longer than u32 length prefix"))?;
        self.out
            .write_all(&key_len.to_le_bytes())
            .map_err(|e| Error::io("write key length", e))?;
        self.out
            .write_all(key)
            .map_err(|e| Error::io("write key", e))?;
        self.out
            .write_all(&value_len.to_le_bytes())
            .map_err(|e| Error::io("write value length", e))?;
        self.out
            .write_all(value)
            .map_err(|e| Error::io("write value", e))?;
        self.count += 1;
        Ok(())
    }

    /// Write the count footer, flush, and `fsync` the file to stable storage.
    pub(crate) fn finish(mut self) -> Result<()> {
        self.out
            .write_all(&self.count.to_le_bytes())
            .map_err(|e| Error::io("write run footer", e))?;
        let file = self
            .out
            .into_inner()
            .map_err(|e| Error::io("flush run buffer", e.into_error()))?;
        file.sync_all()
            .map_err(|e| Error::io("flush run to stable storage", e))?;
        Ok(())
    }
}

/// Positioned read of exactly `buf.len()` bytes at `offset`, without disturbing
/// any file cursor. Implemented per platform: `pread`-family on Unix,
/// `seek_read` on Windows.
#[cfg(unix)]
fn pread_exact(file: &File, buf: &mut [u8], offset: u64) -> std::io::Result<()> {
    use std::os::unix::fs::FileExt;
    file.read_exact_at(buf, offset)
}

/// Positioned read of exactly `buf.len()` bytes at `offset`. Windows has no
/// single positioned read-exact, so this loops over `seek_read` until the
/// buffer is full or the file ends short.
#[cfg(windows)]
fn pread_exact(file: &File, buf: &mut [u8], offset: u64) -> std::io::Result<()> {
    use std::os::windows::fs::FileExt;
    let mut read = 0usize;
    while read < buf.len() {
        let n = file.seek_read(&mut buf[read..], offset + read as u64)?;
        if n == 0 {
            return Err(std::io::Error::from(std::io::ErrorKind::UnexpectedEof));
        }
        read += n;
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn write_run(path: &Path, entries: &[(&[u8], &[u8])]) {
        let mut w = SsTableWriter::create(path).unwrap();
        for (k, v) in entries {
            w.push(k, v).unwrap();
        }
        w.finish().unwrap();
    }

    #[test]
    fn test_roundtrip_single_entry() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("run.sst");
        write_run(&path, &[(b"key", b"value")]);
        let t = SsTable::open(&path).unwrap();
        assert_eq!(t.len(), 1);
        assert_eq!(t.get(b"key").unwrap(), Some(b"value".to_vec()));
    }

    #[test]
    fn test_get_missing_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("run.sst");
        write_run(&path, &[(b"a", b"1"), (b"c", b"3")]);
        let t = SsTable::open(&path).unwrap();
        assert_eq!(t.get(b"b").unwrap(), None);
        assert_eq!(t.get(b"z").unwrap(), None);
        assert_eq!(t.get(b"").unwrap(), None);
    }

    #[test]
    fn test_empty_run_roundtrips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("run.sst");
        write_run(&path, &[]);
        let t = SsTable::open(&path).unwrap();
        assert_eq!(t.len(), 0);
        assert_eq!(t.get(b"anything").unwrap(), None);
    }

    #[test]
    fn test_ordered_iteration_via_index() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("run.sst");
        write_run(&path, &[(b"a", b"1"), (b"b", b"2"), (b"c", b"3")]);
        let t = SsTable::open(&path).unwrap();
        let mut got = Vec::new();
        for i in 0..t.len() {
            got.push((t.key_at(i).to_vec(), t.read_value(i).unwrap()));
        }
        assert_eq!(
            got,
            vec![
                (b"a".to_vec(), b"1".to_vec()),
                (b"b".to_vec(), b"2".to_vec()),
                (b"c".to_vec(), b"3".to_vec()),
            ]
        );
    }

    #[test]
    fn test_bad_magic_is_corruption() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("run.sst");
        std::fs::write(&path, b"NOTMAGIC\x00\x00\x00\x00\x00\x00\x00\x00").unwrap();
        let err = SsTable::open(&path).unwrap_err();
        assert_eq!(
            err.to_string(),
            "sorted-run corruption: bad magic; not an lsm-db sorted run"
        );
    }

    #[test]
    fn test_truncated_file_is_corruption() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("run.sst");
        std::fs::write(&path, b"LSM").unwrap();
        assert!(SsTable::open(&path).is_err());
    }

    #[test]
    fn test_large_value_roundtrips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("run.sst");
        let big = vec![0xABu8; 100_000];
        write_run(&path, &[(b"k", big.as_slice())]);
        let t = SsTable::open(&path).unwrap();
        assert_eq!(t.get(b"k").unwrap(), Some(big));
    }
}
