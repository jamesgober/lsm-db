//! The storage engine.
//!
//! [`Lsm`] ties the in-memory [`MemTable`] and the on-disk [`SsTable`] into the
//! log-structured merge write path: writes accumulate in the buffer; when the
//! buffer reaches its configured capacity it is merged over the previous run and
//! flushed to a new one; reads check the buffer first and fall through to the
//! run.

use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::fs;
use std::ops::{Bound, RangeBounds};
use std::path::{Path, PathBuf};
use std::sync::{RwLock, RwLockReadGuard, RwLockWriteGuard};

use crate::batch::{Batch, Op};
use crate::config::LsmConfig;
use crate::error::{Error, Result};
use crate::memtable::{MemTable, Record};
use crate::scan::Scan;
use crate::sstable::{SsTable, SsTableWriter};

/// Name of the live sorted-run file inside the database directory.
const SSTABLE_FILE: &str = "data.sst";
/// Name of the temporary file a flush writes before atomically installing it.
const SSTABLE_TMP: &str = "data.sst.tmp";

/// Mutable engine state guarded by a single lock.
#[derive(Debug)]
struct Inner {
    memtable: MemTable,
    sstable: Option<SsTable>,
}

/// A log-structured merge-tree key-value store backed by a directory on disk.
///
/// `Lsm` is the Tier-1 entry point: [`open`](Lsm::open), [`put`](Lsm::put),
/// [`get`](Lsm::get), [`delete`](Lsm::delete), and [`scan`](Lsm::scan) cover the
/// whole common case. Keys and values are arbitrary byte strings; keys are
/// ordered lexicographically.
///
/// The handle is cheap to share: every method takes `&self`, and the type is
/// [`Send`] + [`Sync`], so one engine can be wrapped in an
/// [`Arc`](std::sync::Arc) and used from many threads.
///
/// # Durability
///
/// This foundation release flushes complete runs to disk and `fsync`s them, so
/// data that has been flushed survives reopening. Writes still buffered in the
/// memtable when the process exits without a flush are **not** yet crash-safe;
/// write-ahead logging arrives under the `durability` feature in a later
/// release. Call [`flush`](Lsm::flush) to force the buffer to disk on demand.
///
/// # Examples
///
/// ```
/// # fn main() -> Result<(), Box<dyn std::error::Error>> {
/// let dir = tempfile::tempdir()?;
/// let db = lsm_db::Lsm::open(dir.path())?;
///
/// db.put(b"hello", b"world")?;
/// assert_eq!(db.get(b"hello")?, Some(b"world".to_vec()));
///
/// db.delete(b"hello")?;
/// assert_eq!(db.get(b"hello")?, None);
/// # Ok(())
/// # }
/// ```
#[derive(Debug)]
pub struct Lsm {
    dir: PathBuf,
    config: LsmConfig,
    inner: RwLock<Inner>,
}

impl Lsm {
    /// Open the database in `dir`, creating the directory if it does not exist,
    /// using the default [`LsmConfig`].
    ///
    /// Any sorted run left by a previous session is reopened, so flushed data is
    /// visible immediately. A leftover temporary file from a flush that was
    /// interrupted by a crash is discarded — the previous run is still the
    /// authoritative state.
    ///
    /// # Examples
    ///
    /// ```
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// let dir = tempfile::tempdir()?;
    /// let db = lsm_db::Lsm::open(dir.path())?;
    /// db.put(b"k", b"v")?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn open(dir: impl AsRef<Path>) -> Result<Self> {
        Self::open_with(dir, LsmConfig::default())
    }

    /// Open the database in `dir` with an explicit [`LsmConfig`].
    ///
    /// # Examples
    ///
    /// ```
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use lsm_db::{Lsm, LsmConfig};
    /// let dir = tempfile::tempdir()?;
    /// // Flush after every 64 KiB of buffered data.
    /// let db = Lsm::open_with(dir.path(), LsmConfig::new().memtable_capacity(64 * 1024))?;
    /// db.put(b"k", b"v")?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn open_with(dir: impl AsRef<Path>, config: LsmConfig) -> Result<Self> {
        let dir = dir.as_ref().to_path_buf();
        fs::create_dir_all(&dir).map_err(|e| Error::io("create database directory", e))?;

        // Discard a partial run from a flush interrupted before its atomic
        // rename; the previous run (if any) remains the source of truth.
        let tmp = dir.join(SSTABLE_TMP);
        if tmp.exists() {
            fs::remove_file(&tmp).map_err(|e| Error::io("remove stale temporary run", e))?;
        }

        let run_path = dir.join(SSTABLE_FILE);
        let sstable = if run_path.exists() {
            Some(SsTable::open(&run_path)?)
        } else {
            None
        };

        Ok(Lsm {
            dir,
            config,
            inner: RwLock::new(Inner {
                memtable: MemTable::new(),
                sstable,
            }),
        })
    }

    /// Set `key` to `value`, overwriting any previous value.
    ///
    /// The write lands in the in-memory buffer and triggers a flush if the
    /// buffer has reached its configured capacity.
    ///
    /// # Examples
    ///
    /// ```
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// let dir = tempfile::tempdir()?;
    /// let db = lsm_db::Lsm::open(dir.path())?;
    /// db.put(b"key", b"value")?;
    /// assert_eq!(db.get(b"key")?, Some(b"value".to_vec()));
    /// # Ok(())
    /// # }
    /// ```
    pub fn put(&self, key: impl AsRef<[u8]>, value: impl AsRef<[u8]>) -> Result<()> {
        let mut inner = self.write_guard();
        inner
            .memtable
            .put(key.as_ref().to_vec(), value.as_ref().to_vec());
        self.maybe_flush(&mut inner)
    }

    /// Delete `key`. A subsequent [`get`](Lsm::get) returns `None`.
    ///
    /// Deleting a key that is not present is not an error.
    ///
    /// # Examples
    ///
    /// ```
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// let dir = tempfile::tempdir()?;
    /// let db = lsm_db::Lsm::open(dir.path())?;
    /// db.put(b"key", b"value")?;
    /// db.delete(b"key")?;
    /// assert_eq!(db.get(b"key")?, None);
    /// db.delete(b"never-existed")?; // not an error
    /// # Ok(())
    /// # }
    /// ```
    pub fn delete(&self, key: impl AsRef<[u8]>) -> Result<()> {
        let mut inner = self.write_guard();
        inner.memtable.delete(key.as_ref().to_vec());
        self.maybe_flush(&mut inner)
    }

    /// Apply a [`Batch`] of writes as one group.
    ///
    /// The whole batch is applied under a single lock acquisition, so concurrent
    /// readers observe either none or all of it.
    ///
    /// # Examples
    ///
    /// ```
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use lsm_db::Batch;
    /// let dir = tempfile::tempdir()?;
    /// let db = lsm_db::Lsm::open(dir.path())?;
    ///
    /// let mut batch = Batch::new();
    /// batch.put(b"a", b"1");
    /// batch.put(b"b", b"2");
    /// batch.delete(b"c");
    /// db.write(batch)?;
    ///
    /// assert_eq!(db.get(b"a")?, Some(b"1".to_vec()));
    /// assert_eq!(db.get(b"b")?, Some(b"2".to_vec()));
    /// # Ok(())
    /// # }
    /// ```
    pub fn write(&self, batch: Batch) -> Result<()> {
        let mut inner = self.write_guard();
        for (key, op) in batch.into_ops() {
            match op {
                Op::Put(value) => inner.memtable.put(key, value),
                Op::Delete => inner.memtable.delete(key),
            }
        }
        self.maybe_flush(&mut inner)
    }

    /// Look up `key`, returning its value, or `None` if it is absent or deleted.
    ///
    /// The buffer is checked first, then the on-disk run.
    ///
    /// # Examples
    ///
    /// ```
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// let dir = tempfile::tempdir()?;
    /// let db = lsm_db::Lsm::open(dir.path())?;
    /// assert_eq!(db.get(b"missing")?, None);
    /// db.put(b"present", b"1")?;
    /// assert_eq!(db.get(b"present")?, Some(b"1".to_vec()));
    /// # Ok(())
    /// # }
    /// ```
    pub fn get(&self, key: impl AsRef<[u8]>) -> Result<Option<Vec<u8>>> {
        let key = key.as_ref();
        let inner = self.read_guard();
        match inner.memtable.get(key) {
            Some(Record::Value(value)) => Ok(Some(value.clone())),
            Some(Record::Tombstone) => Ok(None),
            None => match inner.sstable.as_ref() {
                Some(table) => table.get(key),
                None => Ok(None),
            },
        }
    }

    /// Iterate the live `(key, value)` pairs whose key falls in `range`, in
    /// ascending key order.
    ///
    /// The range is taken over `Vec<u8>` bounds, so the usual range syntaxes all
    /// work: `..` for everything, `a..b`, `a..=b`, `a..`, `..b`. The returned
    /// [`Scan`] is a consistent snapshot taken when `scan` is called; later
    /// writes do not affect it.
    ///
    /// # Examples
    ///
    /// ```
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// let dir = tempfile::tempdir()?;
    /// let db = lsm_db::Lsm::open(dir.path())?;
    /// db.put(b"a", b"1")?;
    /// db.put(b"b", b"2")?;
    /// db.put(b"c", b"3")?;
    ///
    /// // Half-open range.
    /// let mid: Vec<_> = db.scan(b"a".to_vec()..b"c".to_vec())?.collect();
    /// assert_eq!(mid, vec![(b"a".to_vec(), b"1".to_vec()), (b"b".to_vec(), b"2".to_vec())]);
    ///
    /// // Everything.
    /// assert_eq!(db.scan(..)?.count(), 3);
    /// # Ok(())
    /// # }
    /// ```
    pub fn scan<R>(&self, range: R) -> Result<Scan>
    where
        R: RangeBounds<Vec<u8>>,
    {
        let inner = self.read_guard();
        let entries = collect_range(&inner, &range)?;
        Ok(Scan::new(entries))
    }

    /// Force the in-memory buffer to disk, merging it into the sorted run.
    ///
    /// Flushing an empty buffer is a no-op. After a successful flush every
    /// previously written key is durable and will be read back on reopen.
    ///
    /// # Examples
    ///
    /// ```
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// let dir = tempfile::tempdir()?;
    /// let db = lsm_db::Lsm::open(dir.path())?;
    /// db.put(b"k", b"v")?;
    /// db.flush()?;
    ///
    /// // Reopen and read it back.
    /// drop(db);
    /// let db = lsm_db::Lsm::open(dir.path())?;
    /// assert_eq!(db.get(b"k")?, Some(b"v".to_vec()));
    /// # Ok(())
    /// # }
    /// ```
    pub fn flush(&self) -> Result<()> {
        let mut inner = self.write_guard();
        if inner.memtable.is_empty() {
            return Ok(());
        }
        self.flush_locked(&mut inner)
    }

    /// Flush if the buffer has reached the configured capacity.
    fn maybe_flush(&self, inner: &mut Inner) -> Result<()> {
        if inner.memtable.approx_size() >= self.config.memtable_capacity_bytes()
            && !inner.memtable.is_empty()
        {
            self.flush_locked(inner)?;
        }
        Ok(())
    }

    /// Merge the buffer over the current run into a new run and install it.
    ///
    /// The new run is written to a temporary file and `fsync`ed, the old run's
    /// handle is dropped, and the temporary file is atomically renamed into
    /// place. Dropping the old handle before the rename keeps the operation
    /// valid on Windows, where renaming over an open file is rejected.
    fn flush_locked(&self, inner: &mut Inner) -> Result<()> {
        let memtable = inner.memtable.take();
        let tmp = self.dir.join(SSTABLE_TMP);
        let run_path = self.dir.join(SSTABLE_FILE);

        let mut writer = SsTableWriter::create(&tmp)?;
        merge_into(&mut writer, &memtable, inner.sstable.as_ref())?;
        writer.finish()?;

        inner.sstable = None;
        fs::rename(&tmp, &run_path).map_err(|e| Error::io("install flushed run", e))?;
        inner.sstable = Some(SsTable::open(&run_path)?);
        Ok(())
    }

    /// Acquire the read guard, recovering from a poisoned lock.
    ///
    /// The engine never panics while holding the lock, so poisoning cannot
    /// reflect a torn critical section; recovering the guard is sound and keeps
    /// the library panic-free.
    fn read_guard(&self) -> RwLockReadGuard<'_, Inner> {
        self.inner
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Acquire the write guard, recovering from a poisoned lock. See
    /// [`read_guard`](Self::read_guard) for why recovery is sound.
    fn write_guard(&self) -> RwLockWriteGuard<'_, Inner> {
        self.inner
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// The on-disk key at run index `si`, or `None` past the end.
fn run_key_at(sstable: Option<&SsTable>, si: usize, len: usize) -> Option<&[u8]> {
    match sstable {
        Some(table) if si < len => Some(table.key_at(si)),
        _ => None,
    }
}

/// Merge the buffer over the run, writing every live entry to `writer` in
/// ascending key order. Buffer entries win over run entries with the same key;
/// tombstones drop the key entirely.
fn merge_into(
    writer: &mut SsTableWriter,
    memtable: &BTreeMap<Vec<u8>, Record>,
    sstable: Option<&SsTable>,
) -> Result<()> {
    let run_len = sstable.map_or(0, SsTable::len);
    let mut buffered = memtable.iter().peekable();
    let mut si = 0usize;

    loop {
        let (take_buffered, take_run) = match (buffered.peek(), run_key_at(sstable, si, run_len)) {
            (None, None) => break,
            (Some(_), None) => (true, false),
            (None, Some(_)) => (false, true),
            (Some((bk, _)), Some(rk)) => match bk.as_slice().cmp(rk) {
                Ordering::Less => (true, false),
                Ordering::Greater => (false, true),
                Ordering::Equal => (true, true),
            },
        };

        if take_buffered {
            // `next` advances the cursor even when the record is a tombstone,
            // which the pattern skips writing.
            if let Some((key, Record::Value(value))) = buffered.next() {
                writer.push(key, value)?;
            }
        }
        if take_run {
            if let Some(table) = sstable {
                // When the buffer also covers this key it wins; the shadowed run
                // value is skipped, only the cursor advances.
                if !take_buffered {
                    let value = table.read_value(si)?;
                    writer.push(table.key_at(si), &value)?;
                }
                si += 1;
            }
        }
    }
    Ok(())
}

/// Materialise the live `(key, value)` entries of `inner` whose key lies in
/// `range`, in ascending key order.
fn collect_range<R>(inner: &Inner, range: &R) -> Result<Vec<(Vec<u8>, Vec<u8>)>>
where
    R: RangeBounds<Vec<u8>>,
{
    let sstable = inner.sstable.as_ref();
    let run_len = sstable.map_or(0, SsTable::len);
    let mut buffered = inner.memtable.iter().peekable();
    let mut si = 0usize;
    let mut out = Vec::new();

    loop {
        let (take_buffered, take_run) = match (buffered.peek(), run_key_at(sstable, si, run_len)) {
            (None, None) => break,
            (Some(_), None) => (true, false),
            (None, Some(_)) => (false, true),
            (Some((bk, _)), Some(rk)) => match bk.as_slice().cmp(rk) {
                Ordering::Less => (true, false),
                Ordering::Greater => (false, true),
                Ordering::Equal => (true, true),
            },
        };

        if take_buffered {
            if let Some((key, Record::Value(value))) = buffered.next() {
                if in_range(range, key) {
                    out.push((key.clone(), value.clone()));
                }
            }
        }
        if take_run {
            if let Some(table) = sstable {
                if !take_buffered && in_range(range, table.key_at(si)) {
                    let value = table.read_value(si)?;
                    out.push((table.key_at(si).to_vec(), value));
                }
                si += 1;
            }
        }
    }
    Ok(out)
}

/// Whether `key` satisfies both bounds of `range`.
fn in_range<R: RangeBounds<Vec<u8>>>(range: &R, key: &[u8]) -> bool {
    let after_start = match range.start_bound() {
        Bound::Included(s) => key >= s.as_slice(),
        Bound::Excluded(s) => key > s.as_slice(),
        Bound::Unbounded => true,
    };
    let before_end = match range.end_bound() {
        Bound::Included(e) => key <= e.as_slice(),
        Bound::Excluded(e) => key < e.as_slice(),
        Bound::Unbounded => true,
    };
    after_start && before_end
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn db() -> (tempfile::TempDir, Lsm) {
        let dir = tempfile::tempdir().unwrap();
        let db = Lsm::open(dir.path()).unwrap();
        (dir, db)
    }

    #[test]
    fn test_put_get_roundtrip() {
        let (_d, db) = db();
        db.put(b"k", b"v").unwrap();
        assert_eq!(db.get(b"k").unwrap(), Some(b"v".to_vec()));
    }

    #[test]
    fn test_get_absent_is_none() {
        let (_d, db) = db();
        assert_eq!(db.get(b"absent").unwrap(), None);
    }

    #[test]
    fn test_overwrite_returns_latest() {
        let (_d, db) = db();
        db.put(b"k", b"old").unwrap();
        db.put(b"k", b"new").unwrap();
        assert_eq!(db.get(b"k").unwrap(), Some(b"new".to_vec()));
    }

    #[test]
    fn test_delete_masks_value() {
        let (_d, db) = db();
        db.put(b"k", b"v").unwrap();
        db.delete(b"k").unwrap();
        assert_eq!(db.get(b"k").unwrap(), None);
    }

    #[test]
    fn test_delete_then_put_revives_key() {
        let (_d, db) = db();
        db.put(b"k", b"v1").unwrap();
        db.delete(b"k").unwrap();
        db.put(b"k", b"v2").unwrap();
        assert_eq!(db.get(b"k").unwrap(), Some(b"v2".to_vec()));
    }

    #[test]
    fn test_flush_then_read_from_run() {
        let (_d, db) = db();
        db.put(b"k", b"v").unwrap();
        db.flush().unwrap();
        assert_eq!(db.get(b"k").unwrap(), Some(b"v".to_vec()));
    }

    #[test]
    fn test_buffer_shadows_run_value() {
        let (_d, db) = db();
        db.put(b"k", b"on-disk").unwrap();
        db.flush().unwrap();
        db.put(b"k", b"in-memory").unwrap();
        assert_eq!(db.get(b"k").unwrap(), Some(b"in-memory".to_vec()));
    }

    #[test]
    fn test_delete_after_flush_masks_run_value() {
        let (_d, db) = db();
        db.put(b"k", b"v").unwrap();
        db.flush().unwrap();
        db.delete(b"k").unwrap();
        assert_eq!(db.get(b"k").unwrap(), None);
        // After re-flush the key is gone from disk entirely.
        db.flush().unwrap();
        assert_eq!(db.get(b"k").unwrap(), None);
    }

    #[test]
    fn test_reopen_reads_flushed_keys() {
        let dir = tempfile::tempdir().unwrap();
        {
            let db = Lsm::open(dir.path()).unwrap();
            db.put(b"a", b"1").unwrap();
            db.put(b"b", b"2").unwrap();
            db.flush().unwrap();
        }
        let db = Lsm::open(dir.path()).unwrap();
        assert_eq!(db.get(b"a").unwrap(), Some(b"1".to_vec()));
        assert_eq!(db.get(b"b").unwrap(), Some(b"2".to_vec()));
    }

    #[test]
    fn test_auto_flush_at_capacity() {
        let dir = tempfile::tempdir().unwrap();
        let db = Lsm::open_with(dir.path(), LsmConfig::new().memtable_capacity(0)).unwrap();
        db.put(b"k", b"v").unwrap(); // capacity 0 flushes immediately
        assert!(dir.path().join(SSTABLE_FILE).exists());
        assert_eq!(db.get(b"k").unwrap(), Some(b"v".to_vec()));
    }

    #[test]
    fn test_scan_full_range() {
        let (_d, db) = db();
        db.put(b"c", b"3").unwrap();
        db.put(b"a", b"1").unwrap();
        db.put(b"b", b"2").unwrap();
        let got: Vec<_> = db.scan(..).unwrap().collect();
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
    fn test_scan_half_open_range() {
        let (_d, db) = db();
        for (k, v) in [("a", "1"), ("b", "2"), ("c", "3"), ("d", "4")] {
            db.put(k.as_bytes(), v.as_bytes()).unwrap();
        }
        let got: Vec<_> = db.scan(b"b".to_vec()..b"d".to_vec()).unwrap().collect();
        assert_eq!(
            got,
            vec![
                (b"b".to_vec(), b"2".to_vec()),
                (b"c".to_vec(), b"3".to_vec())
            ]
        );
    }

    #[test]
    fn test_scan_inclusive_range() {
        let (_d, db) = db();
        for (k, v) in [("a", "1"), ("b", "2"), ("c", "3")] {
            db.put(k.as_bytes(), v.as_bytes()).unwrap();
        }
        let got: Vec<_> = db.scan(b"a".to_vec()..=b"b".to_vec()).unwrap().collect();
        assert_eq!(
            got,
            vec![
                (b"a".to_vec(), b"1".to_vec()),
                (b"b".to_vec(), b"2".to_vec())
            ]
        );
    }

    #[test]
    fn test_scan_merges_buffer_and_run() {
        let (_d, db) = db();
        db.put(b"a", b"old-a").unwrap();
        db.put(b"c", b"3").unwrap();
        db.flush().unwrap(); // a, c on disk
        db.put(b"a", b"new-a").unwrap(); // shadows disk
        db.put(b"b", b"2").unwrap();
        db.delete(b"c").unwrap(); // hides disk value
        let got: Vec<_> = db.scan(..).unwrap().collect();
        assert_eq!(
            got,
            vec![
                (b"a".to_vec(), b"new-a".to_vec()),
                (b"b".to_vec(), b"2".to_vec())
            ]
        );
    }

    #[test]
    fn test_batch_applies_all() {
        let (_d, db) = db();
        db.put(b"c", b"keep").unwrap();
        let mut batch = Batch::new();
        batch.put(b"a", b"1");
        batch.put(b"b", b"2");
        batch.delete(b"c");
        db.write(batch).unwrap();
        assert_eq!(db.get(b"a").unwrap(), Some(b"1".to_vec()));
        assert_eq!(db.get(b"b").unwrap(), Some(b"2".to_vec()));
        assert_eq!(db.get(b"c").unwrap(), None);
    }

    #[test]
    fn test_empty_value_roundtrips() {
        let (_d, db) = db();
        db.put(b"k", b"").unwrap();
        assert_eq!(db.get(b"k").unwrap(), Some(Vec::new()));
        db.flush().unwrap();
        assert_eq!(db.get(b"k").unwrap(), Some(Vec::new()));
    }

    #[test]
    fn test_stale_tmp_is_discarded_on_open() {
        let dir = tempfile::tempdir().unwrap();
        {
            let db = Lsm::open(dir.path()).unwrap();
            db.put(b"k", b"v").unwrap();
            db.flush().unwrap();
        }
        // Simulate a crashed flush: a partial temporary file alongside the run.
        std::fs::write(dir.path().join(SSTABLE_TMP), b"garbage").unwrap();
        let db = Lsm::open(dir.path()).unwrap();
        assert_eq!(db.get(b"k").unwrap(), Some(b"v".to_vec()));
        assert!(!dir.path().join(SSTABLE_TMP).exists());
    }

    #[test]
    fn test_engine_is_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<Lsm>();
    }
}
