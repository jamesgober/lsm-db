//! On-disk immutable sorted runs.
//!
//! A *sorted run* (SSTable) is an immutable file holding a contiguous, sorted
//! slice of the key space, each key paired with a [`Record`] (a value or a
//! tombstone). Runs are produced by flushing the memtable and by compaction, and
//! never modified once written.
//!
//! ## On-disk format (v1, frozen for 1.x)
//!
//! The byte layout is normative and specified in `docs/SSTABLE_FORMAT.md`. In
//! summary:
//!
//! ```text
//! ┌──────────────────────┐
//! │ magic "LSMTBL01" (8) │
//! ├──────────────────────┤
//! │ data block 0         │  entries: key_len u32 · key · tag u8 · val_len u32 · val
//! │ data block 1         │  each block holds a sorted run of entries (~4 KiB)
//! │ …                    │
//! ├──────────────────────┤
//! │ index block          │  one record per data block:
//! │                      │    last_key_len u32 · last_key · offset u64 · len u32 · crc u32
//! ├──────────────────────┤
//! │ footer (36 bytes)    │  entry_count u64 · index_offset u64 · index_len u64
//! │                      │  · index_crc u32 · magic (8)
//! └──────────────────────┘
//! ```
//!
//! Every data block carries a CRC32C in the index, so a block is integrity
//! checked when it is read; the index block carries its own CRC32C in the
//! footer. Opening a run reads only the footer and index — values stay on disk
//! and are read one block at a time, with a single positioned read (`pread` on
//! Unix, `seek_read` on Windows) so concurrent readers share one file handle.

use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Write};
use std::ops::Bound;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use crate::bloom::{self, RunFilter};
use crate::cache::{BlockCache, BlockKey, DecodedBlock};
use crate::crc::crc32c;
use crate::error::{Error, Result};
use crate::format::{
    self, BlockEntries, FOOTER_LEN, Footer, IndexEntries, MAGIC, TARGET_BLOCK_SIZE,
};
use crate::record::Record;

/// Process-wide source of unique per-run cache ids. Each opened run gets a fresh
/// id, so block-cache keys never collide across runs within an engine's life.
static NEXT_RUN_ID: AtomicU64 = AtomicU64::new(0);

/// Footer size as a file offset.
const FOOTER_SIZE: u64 = FOOTER_LEN as u64;

/// Per-thread data-block read counter, compiled only for the bloom tests. It
/// lets a test assert that a bloom filter actually cuts the number of blocks
/// read on a negative lookup. Thread-local so parallel tests do not interfere.
#[cfg(all(test, feature = "bloom"))]
pub(crate) mod block_reads {
    use std::cell::Cell;

    thread_local! {
        static COUNT: Cell<u64> = const { Cell::new(0) };
    }

    /// Record one data-block read on the current thread.
    pub(crate) fn record() {
        COUNT.with(|c| c.set(c.get() + 1));
    }

    /// Reset the current thread's counter to zero.
    pub(crate) fn reset() {
        COUNT.with(|c| c.set(0));
    }

    /// The current thread's data-block read count.
    pub(crate) fn count() -> u64 {
        COUNT.with(Cell::get)
    }
}

/// One index record: the last key of a data block and where the block lives.
#[derive(Debug, Clone)]
struct BlockHandle {
    last_key: Vec<u8>,
    offset: u64,
    len: u32,
    crc: u32,
}

/// An open, immutable on-disk sorted run.
///
/// Holds the file handle and the parsed block index; data blocks are read on
/// demand. When dropped while marked [obsolete](SsTable::mark_obsolete), the
/// backing file is removed — so a run superseded by compaction is deleted only
/// once the last reader still using it has finished.
#[derive(Debug)]
pub(crate) struct SsTable {
    file: File,
    path: PathBuf,
    index: Vec<BlockHandle>,
    /// Number of entries (values + tombstones), read from the footer. Used to
    /// size the bloom filter built during compaction.
    entry_count: u64,
    /// Optional per-run bloom filter, attached after open; lets a point read
    /// skip this run when the key is definitely absent.
    filter: Option<RunFilter>,
    /// Shared block cache, attached after open; point lookups serve decoded
    /// blocks from it. `None` means no caching (every lookup decodes directly).
    cache: Option<Arc<BlockCache>>,
    /// Unique id for this run, used as the high half of its block-cache keys.
    run_id: u64,
    obsolete: AtomicBool,
}

impl SsTable {
    /// Open the run at `path`, reading and validating its footer and index.
    ///
    /// Returns [`Error::Corruption`] if the magic is wrong, the footer or index
    /// is inconsistent, or the index checksum does not match.
    pub(crate) fn open(path: &Path) -> Result<Self> {
        let file = File::open(path).map_err(|e| Error::io("open sorted run", e))?;
        let file_len = file
            .metadata()
            .map_err(|e| Error::io("stat sorted run", e))?
            .len();

        if file_len < MAGIC.len() as u64 + FOOTER_SIZE {
            return Err(Error::corruption("file shorter than header and footer"));
        }

        // Footer, read from the end.
        let mut footer = [0u8; FOOTER_LEN];
        pread_exact(&file, &mut footer, file_len - FOOTER_SIZE)
            .map_err(|e| Error::io("read run footer", e))?;
        let Footer {
            entry_count,
            index_offset,
            index_len,
            index_crc,
        } = Footer::parse(&footer, file_len)?;

        // Index block.
        let mut index_bytes = vec![0u8; usize_of(index_len)?];
        pread_exact(&file, &mut index_bytes, index_offset)
            .map_err(|e| Error::io("read run index", e))?;
        if crc32c(&index_bytes) != index_crc {
            return Err(Error::corruption("index checksum mismatch"));
        }

        let index = parse_index(&index_bytes, index_offset)?;
        Ok(SsTable {
            file,
            path: path.to_path_buf(),
            index,
            entry_count,
            filter: None,
            cache: None,
            run_id: NEXT_RUN_ID.fetch_add(1, Ordering::Relaxed),
            obsolete: AtomicBool::new(false),
        })
    }

    /// The run's file name within the database directory (e.g. `run-…​.sst`).
    pub(crate) fn file_name(&self) -> String {
        self.path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default()
    }

    /// The number of entries (values + tombstones) recorded in the run.
    #[inline]
    pub(crate) fn entry_count(&self) -> u64 {
        self.entry_count
    }

    /// Attach a bloom filter to this run (built at write time, or loaded from
    /// the sidecar on reopen). A `None` leaves the run with no filter, so every
    /// lookup consults it directly.
    #[inline]
    pub(crate) fn attach_filter(&mut self, filter: Option<RunFilter>) {
        self.filter = filter;
    }

    /// Attach the engine's shared block cache so point lookups can serve decoded
    /// blocks from it.
    #[inline]
    pub(crate) fn attach_cache(&mut self, cache: Arc<BlockCache>) {
        self.cache = Some(cache);
    }

    /// Whether this run might contain `key`, per its bloom filter. Returns
    /// `true` (consult the run) when there is no filter.
    #[inline]
    pub(crate) fn might_contain(&self, key: &[u8]) -> bool {
        self.filter.as_ref().is_none_or(|f| f.might_contain(key))
    }

    /// Mark the run as superseded; its file is removed when the last [`SsTable`]
    /// handle to it is dropped.
    pub(crate) fn mark_obsolete(&self) {
        self.obsolete.store(true, Ordering::Release);
    }

    /// Look up `key`, returning its [`Record`] if this run contains it.
    ///
    /// The single candidate block is served from the shared block cache when one
    /// is attached, so a repeat lookup over a hot block does no I/O, checksum, or
    /// parse.
    pub(crate) fn lookup(&self, key: &[u8]) -> Result<Option<Record>> {
        let block_idx = self.index.partition_point(|h| h.last_key.as_slice() < key);
        if block_idx >= self.index.len() {
            return Ok(None);
        }
        let block = self.cached_block(block_idx)?;
        Ok(block
            .iter()
            .find(|(k, _)| k.as_slice() == key)
            .map(|(_, r)| r.clone()))
    }

    /// The decoded block at index `i`, from the cache if present, otherwise read,
    /// verified, decoded, and inserted. Used by point lookups.
    fn cached_block(&self, i: usize) -> Result<Arc<DecodedBlock>> {
        let key = BlockKey {
            run_id: self.run_id,
            block_idx: i as u32,
        };
        if let Some(cache) = &self.cache {
            if let Some(block) = cache.get(key) {
                return Ok(block);
            }
            let block = Arc::new(self.decode_block_at(i)?);
            cache.insert(key, Arc::clone(&block));
            return Ok(block);
        }
        Ok(Arc::new(self.decode_block_at(i)?))
    }

    /// Read and decode the data block at index `i`, verifying its checksum. This
    /// is the uncached read; the sequential cursor uses it directly so a scan or
    /// compaction does not pollute the cache.
    fn read_block(&self, i: usize) -> Result<DecodedBlock> {
        self.decode_block_at(i)
    }

    /// The raw positioned read + checksum + decode for block `i`.
    fn decode_block_at(&self, i: usize) -> Result<DecodedBlock> {
        #[cfg(all(test, feature = "bloom"))]
        block_reads::record();
        let handle = &self.index[i];
        let mut buf = vec![0u8; handle.len as usize];
        pread_exact(&self.file, &mut buf, handle.offset)
            .map_err(|e| Error::io("read data block", e))?;
        if crc32c(&buf) != handle.crc {
            return Err(Error::corruption("data block checksum mismatch"));
        }
        decode_block(&buf)
    }

    /// A cursor over every entry in the run, in ascending key order.
    pub(crate) fn cursor(&self) -> RunCursor<'_> {
        RunCursor::new(self)
    }

    /// A cursor that starts at the first block that can hold a key at or
    /// after `start`, found in the block index without reading a block. The
    /// entries of that block below `start` still come out first; the caller
    /// skips them.
    pub(crate) fn cursor_from(&self, start: Bound<&[u8]>) -> RunCursor<'_> {
        let mut cursor = RunCursor::new(self);
        cursor.next_block = match start {
            Bound::Included(key) | Bound::Excluded(key) => {
                self.index.partition_point(|h| h.last_key.as_slice() < key)
            }
            Bound::Unbounded => 0,
        };
        cursor
    }
}

impl Drop for SsTable {
    fn drop(&mut self) {
        if self.obsolete.load(Ordering::Acquire) {
            // Best effort: the run is already out of the live set, so a failed
            // unlink only leaves a file that the next open will reclaim as an
            // orphan. Closing the handle first keeps this valid on Windows. The
            // bloom sidecar is removed alongside the run it describes.
            let _ = std::fs::remove_file(&self.path);
            let _ = std::fs::remove_file(bloom::sidecar_path(&self.path));
        }
    }
}

/// A peeking cursor over a run's entries, reading one block at a time.
///
/// Block read or decode errors are captured and surfaced through
/// [`error`](RunCursor::error); once an error occurs the cursor reports no more
/// entries, so a merge can drain every cursor and then check them all.
#[derive(Debug)]
pub(crate) struct RunCursor<'a> {
    table: &'a SsTable,
    next_block: usize,
    block: std::vec::IntoIter<(Vec<u8>, Record)>,
    peeked: Option<(Vec<u8>, Record)>,
    error: Option<Error>,
}

impl<'a> RunCursor<'a> {
    fn new(table: &'a SsTable) -> Self {
        RunCursor {
            table,
            next_block: 0,
            block: Vec::new().into_iter(),
            peeked: None,
            error: None,
        }
    }

    /// Ensure `peeked` holds the next entry, loading blocks as needed.
    fn fill(&mut self) {
        if self.peeked.is_some() || self.error.is_some() {
            return;
        }
        loop {
            if let Some(entry) = self.block.next() {
                self.peeked = Some(entry);
                return;
            }
            if self.next_block >= self.table.index.len() {
                return; // exhausted
            }
            match self.table.read_block(self.next_block) {
                Ok(entries) => {
                    self.next_block += 1;
                    self.block = entries.into_iter();
                }
                Err(e) => {
                    self.error = Some(e);
                    return;
                }
            }
        }
    }

    /// The key of the next entry, or `None` if exhausted or errored.
    pub(crate) fn peek_key(&mut self) -> Option<&[u8]> {
        self.fill();
        self.peeked.as_ref().map(|(k, _)| k.as_slice())
    }

    /// Take the next entry, advancing the cursor.
    pub(crate) fn next_entry(&mut self) -> Option<(Vec<u8>, Record)> {
        self.fill();
        self.peeked.take()
    }

    /// The error that stopped the cursor, if any.
    #[cfg(test)]
    pub(crate) fn error(&self) -> Option<&Error> {
        self.error.as_ref()
    }

    /// Take the error that stopped the cursor, if any, leaving it cleared.
    pub(crate) fn take_error(&mut self) -> Option<Error> {
        self.error.take()
    }
}

/// Streaming writer for a sorted run. Entries must be supplied in strictly
/// ascending key order; the merge that feeds it guarantees that.
pub(crate) struct SsTableWriter {
    out: BufWriter<File>,
    offset: u64,
    block_buf: Vec<u8>,
    block_last_key: Vec<u8>,
    index: Vec<BlockHandle>,
    entry_count: u64,
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
        Ok(SsTableWriter {
            out,
            offset: MAGIC.len() as u64,
            block_buf: Vec::with_capacity(TARGET_BLOCK_SIZE + 256),
            block_last_key: Vec::new(),
            index: Vec::new(),
            entry_count: 0,
        })
    }

    /// Append one entry. Keys must arrive in strictly ascending order.
    pub(crate) fn push(&mut self, key: &[u8], record: &Record) -> Result<()> {
        // The engine's write methods already enforce these limits; checking
        // again here guarantees a run is never written that a reader rejects.
        let entry = record.as_entry();
        format::check_lengths(key, entry)?;
        format::encode_entry(&mut self.block_buf, key, entry);
        self.block_last_key.clear();
        self.block_last_key.extend_from_slice(key);
        self.entry_count += 1;

        if self.block_buf.len() >= TARGET_BLOCK_SIZE {
            self.flush_block()?;
        }
        Ok(())
    }

    /// Write the current block to disk and record its index handle.
    fn flush_block(&mut self) -> Result<()> {
        if self.block_buf.is_empty() {
            return Ok(());
        }
        let crc = crc32c(&self.block_buf);
        let len = u32::try_from(self.block_buf.len())
            .map_err(|_| Error::corruption("data block larger than u32"))?;
        self.out
            .write_all(&self.block_buf)
            .map_err(|e| Error::io("write data block", e))?;
        self.index.push(BlockHandle {
            last_key: std::mem::take(&mut self.block_last_key),
            offset: self.offset,
            len,
            crc,
        });
        self.offset += u64::from(len);
        self.block_buf.clear();
        Ok(())
    }

    /// Flush the final block, write the index and footer, and `fsync`.
    pub(crate) fn finish(mut self) -> Result<()> {
        self.flush_block()?;

        let index_offset = self.offset;
        let mut index_bytes = Vec::new();
        for handle in &self.index {
            format::encode_index_entry(
                &mut index_bytes,
                &handle.last_key,
                handle.offset,
                handle.len,
                handle.crc,
            );
        }
        let index_crc = crc32c(&index_bytes);
        let index_len = index_bytes.len() as u64;
        self.out
            .write_all(&index_bytes)
            .map_err(|e| Error::io("write run index", e))?;

        let footer = format::encode_footer(self.entry_count, index_offset, index_len, index_crc);
        self.out
            .write_all(&footer)
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

/// Decode an index block into block handles, validating bounds, ordering, and
/// that every block lies inside the data region `[8, data_end)`.
fn parse_index(bytes: &[u8], data_end: u64) -> Result<Vec<BlockHandle>> {
    IndexEntries::new(bytes, data_end)
        .map(|entry| {
            entry.map(|e| BlockHandle {
                last_key: e.last_key.to_vec(),
                offset: e.offset,
                len: e.len,
                crc: e.crc,
            })
        })
        .collect()
}

/// Decode a data block into its entries, validating bounds and ordering.
fn decode_block(bytes: &[u8]) -> Result<DecodedBlock> {
    let mut entries = Vec::new();
    for item in BlockEntries::new(bytes) {
        let (key, entry) = item?;
        entries.push((key.to_vec(), Record::from(entry)));
    }
    Ok(entries)
}

/// Convert an on-disk `u64` length to `usize`, rejecting values too large for
/// the platform rather than truncating.
fn usize_of(len: u64) -> Result<usize> {
    usize::try_from(len).map_err(|_| Error::corruption("length exceeds platform usize"))
}

/// Positioned read of exactly `buf.len()` bytes at `offset` (Unix `pread`).
#[cfg(unix)]
fn pread_exact(file: &File, buf: &mut [u8], offset: u64) -> std::io::Result<()> {
    use std::os::unix::fs::FileExt;
    file.read_exact_at(buf, offset)
}

/// Positioned read of exactly `buf.len()` bytes at `offset` (Windows
/// `seek_read`, looped to fill the buffer).
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

    fn write_run(path: &Path, entries: &[(&[u8], Record)]) {
        let mut w = SsTableWriter::create(path).unwrap();
        for (k, r) in entries {
            w.push(k, r).unwrap();
        }
        w.finish().unwrap();
    }

    fn val(v: &[u8]) -> Record {
        Record::Value(v.to_vec())
    }

    fn count(t: &SsTable) -> usize {
        let mut cur = t.cursor();
        let mut n = 0;
        while cur.next_entry().is_some() {
            n += 1;
        }
        n
    }

    #[test]
    fn test_roundtrip_value() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("run.sst");
        write_run(&path, &[(b"key", val(b"value"))]);
        let t = SsTable::open(&path).unwrap();
        assert_eq!(count(&t), 1);
        assert_eq!(t.lookup(b"key").unwrap(), Some(val(b"value")));
    }

    #[test]
    fn test_roundtrip_tombstone() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("run.sst");
        write_run(&path, &[(b"gone", Record::Tombstone)]);
        let t = SsTable::open(&path).unwrap();
        assert_eq!(t.lookup(b"gone").unwrap(), Some(Record::Tombstone));
    }

    #[test]
    fn test_lookup_missing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("run.sst");
        write_run(&path, &[(b"a", val(b"1")), (b"c", val(b"3"))]);
        let t = SsTable::open(&path).unwrap();
        assert_eq!(t.lookup(b"b").unwrap(), None);
        assert_eq!(t.lookup(b"z").unwrap(), None);
        assert_eq!(t.lookup(b"").unwrap(), None);
    }

    #[test]
    fn test_empty_run() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("run.sst");
        write_run(&path, &[]);
        let t = SsTable::open(&path).unwrap();
        assert_eq!(count(&t), 0);
        assert_eq!(t.lookup(b"anything").unwrap(), None);
        assert!(t.cursor().peek_key().is_none());
    }

    #[test]
    fn test_multi_block_roundtrip_and_cursor() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("run.sst");
        // Each value ~200 bytes; thousands of entries force many blocks.
        let mut entries = Vec::new();
        for i in 0..5_000u32 {
            entries.push((format!("key{i:06}").into_bytes(), val(&[b'x'; 200])));
        }
        let refs: Vec<(&[u8], Record)> = entries
            .iter()
            .map(|(k, r)| (k.as_slice(), r.clone()))
            .collect();
        write_run(&path, &refs);

        let t = SsTable::open(&path).unwrap();
        assert!(t.index.len() > 1, "expected multiple blocks");
        assert_eq!(count(&t), 5_000);

        // Random lookups.
        assert_eq!(t.lookup(b"key000000").unwrap(), Some(val(&[b'x'; 200])));
        assert_eq!(t.lookup(b"key004999").unwrap(), Some(val(&[b'x'; 200])));
        assert_eq!(t.lookup(b"key005000").unwrap(), None);

        // Cursor yields all entries in order.
        let mut cur = t.cursor();
        let mut count = 0u32;
        let mut last: Option<Vec<u8>> = None;
        while let Some((k, _)) = cur.next_entry() {
            if let Some(p) = last {
                assert!(p < k);
            }
            last = Some(k);
            count += 1;
        }
        assert!(cur.error().is_none());
        assert_eq!(count, 5_000);
    }

    #[test]
    fn test_large_value_single_block() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("run.sst");
        let big = vec![0xABu8; 100_000];
        write_run(&path, &[(b"k", val(&big))]);
        let t = SsTable::open(&path).unwrap();
        assert_eq!(t.lookup(b"k").unwrap(), Some(val(&big)));
    }

    #[test]
    fn test_bad_magic_is_corruption() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("run.sst");
        std::fs::write(&path, vec![0u8; 64]).unwrap();
        assert!(SsTable::open(&path).is_err());
    }

    #[test]
    fn test_corrupted_block_detected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("run.sst");
        write_run(&path, &[(b"a", val(b"hello")), (b"b", val(b"world"))]);
        // Flip a byte in the first data block (just past the magic).
        let mut bytes = std::fs::read(&path).unwrap();
        bytes[10] ^= 0xFF;
        std::fs::write(&path, &bytes).unwrap();
        let t = SsTable::open(&path).unwrap(); // index still valid
        assert!(matches!(t.lookup(b"a"), Err(Error::Corruption { .. })));
    }

    #[test]
    fn test_obsolete_drop_removes_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("run.sst");
        write_run(&path, &[(b"k", val(b"v"))]);
        let t = SsTable::open(&path).unwrap();
        t.mark_obsolete();
        drop(t);
        assert!(!path.exists());
    }

    #[test]
    fn test_non_obsolete_drop_keeps_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("run.sst");
        write_run(&path, &[(b"k", val(b"v"))]);
        let t = SsTable::open(&path).unwrap();
        drop(t);
        assert!(path.exists());
    }

    /// The in-memory `RunWriter` and the engine's file writer must produce the
    /// same bytes, and each side must read what the other wrote.
    #[test]
    fn test_file_writer_and_run_writer_are_byte_identical() {
        use crate::format::{Entry, RunReader, RunWriter};

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("run.sst");
        let mut entries = Vec::new();
        for i in 0..3_000u32 {
            let key = format!("key{i:06}").into_bytes();
            let rec = if i % 7 == 0 {
                Record::Tombstone
            } else {
                val(&vec![b'v'; (i % 300) as usize])
            };
            entries.push((key, rec));
        }
        let refs: Vec<(&[u8], Record)> = entries
            .iter()
            .map(|(k, r)| (k.as_slice(), r.clone()))
            .collect();
        write_run(&path, &refs);

        let mut w = RunWriter::new();
        for (k, r) in &entries {
            match r {
                Record::Value(v) => w.put(k, v).unwrap(),
                Record::Tombstone => w.delete(k).unwrap(),
            }
        }
        let in_memory = w.finish();
        let on_disk = std::fs::read(&path).unwrap();
        assert_eq!(in_memory, on_disk);

        // RunReader reads the engine's file ...
        let run = RunReader::new(&on_disk).unwrap();
        assert_eq!(run.entry_count(), 3_000);
        let read: Vec<(Vec<u8>, Record)> = run
            .iter()
            .map(|r| {
                let (k, e) = r.unwrap();
                (k.to_vec(), Record::from(e))
            })
            .collect();
        assert_eq!(read, entries);
        assert_eq!(run.get(b"key000007").unwrap(), Some(Entry::Tombstone));

        // ... and the engine opens the RunWriter's output.
        let path2 = dir.path().join("run2.sst");
        std::fs::write(&path2, &in_memory).unwrap();
        let t = SsTable::open(&path2).unwrap();
        assert_eq!(count(&t), 3_000);
        assert_eq!(t.lookup(b"key000001").unwrap(), Some(val(b"v")));
        assert_eq!(t.lookup(b"key000007").unwrap(), Some(Record::Tombstone));
    }

    #[test]
    fn test_index_pointing_outside_data_region_is_corruption() {
        use crate::format::{Entry, encode_entry, encode_footer, encode_index_entry};

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("run.sst");
        let mut out = MAGIC.to_vec();
        let mut block = Vec::new();
        encode_entry(&mut block, b"k", Entry::Value(b"v"));
        out.extend_from_slice(&block);
        let index_offset = out.len() as u64;
        let mut index = Vec::new();
        // Claims the block runs into the index.
        encode_index_entry(&mut index, b"k", 8, block.len() as u32 + 4, crc32c(&block));
        out.extend_from_slice(&index);
        out.extend_from_slice(&encode_footer(
            1,
            index_offset,
            index.len() as u64,
            crc32c(&index),
        ));
        std::fs::write(&path, &out).unwrap();
        assert!(matches!(
            SsTable::open(&path),
            Err(Error::Corruption { .. })
        ));
    }
}
