//! The storage engine.
//!
//! [`Lsm`] ties the in-memory memtable and the on-disk runs into the
//! log-structured merge write path. Writes accumulate in the memtable; when it
//! fills it is flushed to a new sorted run; a background thread compacts the runs
//! into one when they grow too numerous. Reads consult the memtable, then each
//! run newest first, merging where a range is requested.
//!
//! ## Concurrency
//!
//! Engine state — the memtable and the ordered list of runs — lives behind a
//! single [`RwLock`]. Reads snapshot what they need (a value, or the run handles
//! and a memtable slice) under the read lock, then do their file I/O without
//! holding it. Writes take the write lock. A flush holds the write lock while
//! it writes the memtable as a run and installs it, so reads and writes wait
//! for a flush (its time grows with the memtable's size), and with the
//! `durability` feature every write holds it across its log `fsync`.
//! Compaction does its expensive merge with no lock held and takes the write
//! lock only to swap the finished run in and write the manifest (one small
//! file and its `fsync`). A run removed by
//! compaction is reference counted: its file is deleted only when the last reader
//! still holding it has finished (see [`SsTable`]'s `Drop`).

use std::collections::HashSet;
use std::fs;
use std::ops::{Bound, RangeBounds};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, RwLock, RwLockReadGuard, RwLockWriteGuard};
use std::thread::JoinHandle;

use crate::batch::{Batch, Op};
use crate::bloom::{self, RunFilter};
use crate::cache::BlockCache;
use crate::config::LsmConfig;
use crate::durability::Durability;
use crate::error::{Error, Result};
use crate::format;
use crate::lock::{DirLock, LOCK_FILE};
use crate::manifest::{self, Manifest};
use crate::memtable::MemTable;
use crate::merge::Merge;
use crate::record::Record;
use crate::scan::Scan;
use crate::sstable::{SsTable, SsTableWriter};

/// Mutable engine state guarded by one lock.
#[derive(Debug)]
struct Inner {
    memtable: MemTable,
    /// Live runs, newest first.
    runs: Vec<Arc<SsTable>>,
    /// Write-ahead log (a no-op unless the `durability` feature is enabled).
    durability: Durability,
}

/// Coordination state for the background compactor.
#[derive(Debug, Default)]
struct CompactionState {
    /// A compaction has been requested and not yet started.
    pending: bool,
    /// A compaction is currently running.
    running: bool,
    /// The engine is shutting down; the compactor should exit.
    shutdown: bool,
    /// Bumped after every completed compaction attempt, so waiters can observe
    /// progress.
    generation: u64,
}

/// Shared engine internals, owned jointly by the handle and the compactor thread.
#[derive(Debug)]
struct Engine {
    dir: PathBuf,
    config: LsmConfig,
    inner: RwLock<Inner>,
    /// Next run sequence number to allocate. Atomic so a compaction can reserve
    /// its output name without holding the engine lock.
    next_seq: AtomicU64,
    /// Guards against two compactions overlapping.
    compacting: AtomicBool,
    compaction: Mutex<CompactionState>,
    cond: Condvar,
    /// The last error a background compaction produced, if any.
    last_error: Mutex<Option<Error>>,
    /// Shared cache of decoded data blocks, consulted by point lookups across
    /// every run.
    cache: Arc<BlockCache>,
    /// Make the next compaction of this engine panic. Test-only.
    #[cfg(test)]
    inject_panic: AtomicBool,
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
/// A background thread compacts runs as they accumulate. Dropping the `Lsm`
/// signals that thread to stop and joins it, so no work outlives the handle.
///
/// # Durability
///
/// Flushed runs are `fsync`ed and recorded in a manifest, so flushed data
/// survives reopening and a crash mid-flush or mid-compaction recovers to a
/// consistent run set. Dropping the handle, or [`close`](Lsm::close), flushes
/// the in-memory buffer, so a clean exit keeps every write. A crash (the
/// process killed, the machine losing power) before a flush loses the writes
/// still buffered, unless the `durability` feature is on: it logs each write
/// before acknowledging it and replays the log on open. Call
/// [`flush`](Lsm::flush) to force the buffer to disk on demand.
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
    engine: Arc<Engine>,
    compactor: Option<JoinHandle<()>>,
    /// Held for the handle's life; dropped last, after the final flush.
    _lock: DirLock,
}

/// Where [`Lsm::rebuild_manifest`] keeps a manifest it replaces.
const MANIFEST_OLD: &str = "MANIFEST.old";

/// Name of the write-ahead log file (`durability`), which a store that never
/// flushed may hold without a manifest.
const WAL_FILE: &str = "wal.log";

/// Check that `dir`, which has no manifest, holds no store: nothing but the
/// lock file, temporary files, and a write-ahead log (a store from before
/// 1.3, which wrote its manifest at the first flush, may have only a log).
fn check_new_store(dir: &Path) -> Result<()> {
    let mut foreign = false;
    for entry in fs::read_dir(dir).map_err(|e| Error::io("scan database directory", e))? {
        let entry = entry.map_err(|e| Error::io("read directory entry", e))?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if manifest::seq_of(&name).is_some() {
            return Err(Error::corruption(
                "MANIFEST is missing but run files exist; restore it from a backup or rebuild it \
                 with Lsm::rebuild_manifest",
            ));
        }
        if !(name == LOCK_FILE || name == WAL_FILE || name.ends_with(".tmp")) {
            foreign = true;
        }
    }
    if foreign {
        return Err(Error::invalid_input(
            "the directory has no MANIFEST and is not empty; a new store needs an empty \
             directory",
        ));
    }
    Ok(())
}

impl Lsm {
    /// Open the database in `dir`, creating the directory if it does not exist,
    /// using the default [`LsmConfig`].
    ///
    /// The run set recorded in the manifest is reopened, so flushed data is
    /// visible immediately. Temporary files and run files orphaned by a crash
    /// mid-flush or mid-compaction are reclaimed.
    ///
    /// The handle holds the directory: a `LOCK` file in it is locked until the
    /// handle is dropped or [`close`](Lsm::close)d, and a second open of the
    /// directory, in this process or another, fails until then. A directory
    /// without a `MANIFEST` is a new store only when it is empty (a `LOCK`
    /// file, temporary files, and a `wal.log` are allowed); otherwise the open
    /// is refused and nothing in the directory is changed or removed.
    ///
    /// # Errors
    ///
    /// - [`Error::Io`] whose source has kind
    ///   [`WouldBlock`](std::io::ErrorKind::WouldBlock) when another handle
    ///   has the directory open; [`Error::Io`] when the directory or a file
    ///   in it cannot be read or written.
    /// - [`Error::Corruption`] when the manifest is malformed, names a run
    ///   that is missing or fails its checks, or is missing while run files
    ///   exist (see [`rebuild_manifest`](Lsm::rebuild_manifest)), and, with
    ///   `durability`, when the write-ahead log is damaged before its last
    ///   record.
    /// - [`Error::InvalidInput`] when a directory without a manifest holds
    ///   other files, and, in a build without `durability`, when the
    ///   directory holds a non-empty `wal.log`: writes logged by a durable
    ///   build that this build cannot replay. Open it with the `durability`
    ///   feature first.
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
    /// let db = Lsm::open_with(dir.path(), LsmConfig::new().memtable_capacity(64 * 1024))?;
    /// db.put(b"k", b"v")?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn open_with(dir: impl AsRef<Path>, config: LsmConfig) -> Result<Self> {
        let dir = dir.as_ref().to_path_buf();
        fs::create_dir_all(&dir).map_err(|e| Error::io("create database directory", e))?;

        // The checks that can refuse a directory run first, reading only, so
        // a refused directory is left as it was. They run again below with
        // the lock held, which is what makes them hold.
        Durability::check(&dir)?;
        if Manifest::load(&dir)?.is_none() {
            check_new_store(&dir)?;
        }

        // One handle per directory: a second one would hand out the same run
        // names and overwrite this one's runs and manifest.
        let lock = DirLock::acquire(&dir)?;

        // Without the `durability` feature, a log this build cannot replay
        // refuses the open.
        Durability::check(&dir)?;
        let manifest = Manifest::load(&dir)?;
        if manifest.is_none() {
            // Every store writes its manifest when it is created, so a
            // directory without one is a new store only when it is empty.
            check_new_store(&dir)?;
        }
        // Open the write-ahead log before anything in the directory changes,
        // so a damaged log refuses the open with nothing removed.
        let mut durability = Durability::open(&dir)?;
        let (run_names, manifest_seq) = match manifest {
            Some(m) => (m.runs, m.next_seq),
            None => {
                Manifest::store(&dir, 0, &[])?;
                (Vec::new(), 0)
            }
        };
        let live: HashSet<&str> = run_names.iter().map(String::as_str).collect();
        if live.len() != run_names.len() {
            return Err(Error::corruption("manifest names a run twice"));
        }

        // Shared block cache for point lookups, sized from the config.
        let cache = BlockCache::new(config.block_cache_capacity_bytes());

        // Open the live runs in recency order, attaching each run's bloom filter
        // from its sidecar (a no-op without the `bloom` feature, and a tolerated
        // miss if a sidecar is absent — the run is simply consulted directly) and
        // the shared block cache.
        let mut runs = Vec::with_capacity(run_names.len());
        for name in &run_names {
            let path = dir.join(name);
            let mut table = match SsTable::open(&path) {
                Ok(table) => table,
                Err(Error::Io { source, .. }) if source.kind() == std::io::ErrorKind::NotFound => {
                    return Err(Error::corruption("manifest references a missing run"));
                }
                Err(err) => return Err(err),
            };
            // A filter that fails its checks, or answers "absent" for a key
            // the run's index names (a sidecar hashed another way), is
            // dropped and rebuilt from the run.
            let filter = match RunFilter::load(&path)? {
                bloom::Sidecar::Loaded(filter) if table.filter_covers_index(&filter) => {
                    Some(filter)
                }
                bloom::Sidecar::Loaded(_) | bloom::Sidecar::Rejected => {
                    let _ = std::fs::remove_file(bloom::sidecar_path(&path));
                    let rebuilt = table.rebuild_filter();
                    if let Some(filter) = &rebuilt {
                        // A sidecar that cannot be written is rebuilt again
                        // at the next open; the filter in memory serves now.
                        let _ = filter.write_sidecar(&path);
                    }
                    rebuilt
                }
                bloom::Sidecar::Absent => None,
            };
            table.attach_filter(filter);
            table.attach_cache(Arc::clone(&cache));
            runs.push(Arc::new(table));
        }

        // Reclaim orphans (temporaries and runs no longer in the manifest) and
        // make sure the next sequence number is past every file on disk. The
        // manifest was read and every run it names opened, so a run file it
        // does not name is not live.
        let mut next_seq = manifest_seq;
        let mut removed = false;
        for entry in fs::read_dir(&dir).map_err(|e| Error::io("scan database directory", e))? {
            let entry = entry.map_err(|e| Error::io("read directory entry", e))?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.ends_with(".tmp") {
                fs::remove_file(entry.path()).map_err(|e| Error::io("remove temporary file", e))?;
                removed = true;
            } else if let Some(run) = name.strip_suffix(".bloom") {
                // A bloom sidecar whose run is not live is an orphan (e.g. from a
                // compaction that crashed before its manifest commit).
                if !live.contains(run) {
                    fs::remove_file(entry.path())
                        .map_err(|e| Error::io("remove orphan bloom sidecar", e))?;
                    removed = true;
                }
            } else if let Some(seq) = manifest::seq_of(&name) {
                let after = seq
                    .checked_add(1)
                    .ok_or_else(|| Error::corruption("run sequence number at its maximum"))?;
                next_seq = next_seq.max(after);
                if !live.contains(name.as_str()) {
                    fs::remove_file(entry.path()).map_err(|e| Error::io("remove orphan run", e))?;
                    removed = true;
                }
            }
        }
        if removed {
            crate::fault::sync_dir(&dir)?;
        }

        // Replay any writes not yet flushed into a fresh memtable. With the
        // `durability` feature off this is a no-op.
        let mut memtable = MemTable::new();
        durability.replay(&mut memtable)?;

        let engine = Arc::new(Engine {
            dir,
            config,
            inner: RwLock::new(Inner {
                memtable,
                runs,
                durability,
            }),
            next_seq: AtomicU64::new(next_seq),
            compacting: AtomicBool::new(false),
            compaction: Mutex::new(CompactionState::default()),
            cond: Condvar::new(),
            last_error: Mutex::new(None),
            cache,
            #[cfg(test)]
            inject_panic: AtomicBool::new(false),
        });

        // Checkpoint recovered writes: persist them as a run and empty the log,
        // so recovery only ever replays the writes since the most recent flush.
        engine.flush()?;

        let compactor = {
            let engine = Arc::clone(&engine);
            std::thread::Builder::new()
                .name("lsm-compactor".to_owned())
                .spawn(move || compactor_loop(&engine))
                .map_err(|e| Error::io("spawn compaction thread", e))?
        };

        Ok(Lsm {
            engine,
            compactor: Some(compactor),
            _lock: lock,
        })
    }

    /// Write a new manifest for the store in `dir` from the run files in it,
    /// for a store whose manifest is lost or damaged. Returns the number of
    /// runs the new manifest names.
    ///
    /// [`open`](Lsm::open) refuses a directory that holds run files but no
    /// manifest, and deletes nothing. This is the explicit recovery: every
    /// `run-<seq>.sst` file is opened and checked, and the runs are recorded
    /// newest first by their sequence number. That is the order the engine
    /// gives them: a flush or compaction takes the next number when it
    /// starts, and a compaction merges every run that exists at that moment.
    /// A damaged manifest is kept as `MANIFEST.old`. The write-ahead log
    /// (`durability`) is left as it is; the next open replays it over the
    /// runs.
    ///
    /// The directory may still hold run files that a flush or compaction
    /// left when it failed before its commit; an open would have removed
    /// them, and the rebuilt manifest names them too. A compaction's leftover
    /// output only repeats its inputs. A failed flush's leftover run holds
    /// writes that the engine kept in memory and wrote again in a later run,
    /// so the leftover is older than that run; but a key in it that was
    /// deleted later and dropped by a compaction since then reads back.
    /// Check the data after a rebuild.
    ///
    /// # Errors
    ///
    /// - [`Error::InvalidInput`] if `dir` has a manifest that reads and whose
    ///   runs all exist: there is nothing to rebuild, and a rebuild would
    ///   bring back run files that the manifest dropped.
    /// - [`Error::Corruption`] if a run file fails its checks. Nothing is
    ///   written, so the file can be moved aside and the rebuild run again.
    /// - [`Error::Io`] if the directory or an existing manifest cannot be read
    ///   (an unreadable manifest is not replaced) or the new manifest cannot be
    ///   written, and the error [`open`](Lsm::open) returns when another
    ///   handle has the directory open.
    ///
    /// # Examples
    ///
    /// ```
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// let dir = tempfile::tempdir()?;
    /// let db = lsm_db::Lsm::open(dir.path())?;
    /// db.put(b"k", b"v")?;
    /// db.close()?;
    ///
    /// std::fs::remove_file(dir.path().join("MANIFEST"))?;
    /// assert!(lsm_db::Lsm::open(dir.path()).is_err());
    ///
    /// assert_eq!(lsm_db::Lsm::rebuild_manifest(dir.path())?, 1);
    /// let db = lsm_db::Lsm::open(dir.path())?;
    /// assert_eq!(db.get(b"k")?, Some(b"v".to_vec()));
    /// # Ok(())
    /// # }
    /// ```
    pub fn rebuild_manifest(dir: impl AsRef<Path>) -> Result<usize> {
        let dir = dir.as_ref();
        let _lock = DirLock::acquire(dir)?;

        match Manifest::load(dir) {
            Ok(Some(manifest)) => {
                let mut all_present = true;
                for name in &manifest.runs {
                    match fs::metadata(dir.join(name)) {
                        Ok(_) => {}
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => all_present = false,
                        Err(e) => return Err(Error::io("stat run file", e)),
                    }
                }
                if all_present {
                    return Err(Error::invalid_input(
                        "the manifest is intact; there is nothing to rebuild",
                    ));
                }
            }
            // Missing or damaged: rebuild.
            Ok(None) | Err(Error::Corruption { .. }) => {}
            // A manifest that could not be read is not replaced.
            Err(err) => return Err(err),
        }

        let mut seqs = Vec::new();
        for entry in fs::read_dir(dir).map_err(|e| Error::io("scan database directory", e))? {
            let entry = entry.map_err(|e| Error::io("read directory entry", e))?;
            if let Some(seq) = manifest::seq_of(&entry.file_name().to_string_lossy()) {
                seqs.push(seq);
            }
        }
        // Newest first.
        seqs.sort_unstable_by(|a, b| b.cmp(a));
        let next_seq = match seqs.first() {
            Some(&newest) => newest
                .checked_add(1)
                .ok_or_else(|| Error::corruption("run sequence number at its maximum"))?,
            None => 0,
        };
        let names: Vec<String> = seqs
            .iter()
            .map(|&seq| manifest::run_filename(seq))
            .collect();
        for name in &names {
            // The footer and index checks every open makes; each block is
            // checked when it is read.
            let _ = SsTable::open(&dir.join(name))?;
        }

        let current = dir.join(manifest::MANIFEST);
        match fs::metadata(&current) {
            Ok(_) => fs::rename(&current, dir.join(MANIFEST_OLD))
                .map_err(|e| Error::io("keep the old manifest", e))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(Error::io("stat manifest", e)),
        }
        Manifest::store(dir, next_seq, &names)?;
        Ok(names.len())
    }

    /// Take the error the most recent failed background compaction returned,
    /// if one failed since the last call; `None` when none has.
    ///
    /// Compaction runs on its own thread, so its errors (a full disk, a
    /// damaged run, a panic inside the merge) reach no caller directly. A
    /// failed compaction changes nothing: every run it read stays live, the
    /// files it wrote are removed, and the next flush that reaches the
    /// trigger tries again. Reads and writes go on working. Poll this to
    /// notice a compaction that keeps failing, for example while the number
    /// of runs, and so the cost of a read, grows.
    ///
    /// # Examples
    ///
    /// ```
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// let dir = tempfile::tempdir()?;
    /// let db = lsm_db::Lsm::open(dir.path())?;
    /// assert!(db.take_compaction_error().is_none());
    /// # Ok(())
    /// # }
    /// ```
    #[must_use]
    pub fn take_compaction_error(&self) -> Option<Error> {
        self.engine
            .last_error
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take()
    }

    /// Set `key` to `value`, overwriting any previous value.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidInput`] if `key` is longer than
    /// [`MAX_KEY_LEN`](crate::format::MAX_KEY_LEN) or `value` is longer than
    /// [`MAX_VALUE_LEN`](crate::format::MAX_VALUE_LEN) (both 1 GiB); nothing is
    /// written. [`Error::Io`] if logging the write (with `durability`) or a
    /// resulting flush fails.
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
        self.engine.put(key.as_ref(), value.as_ref())
    }

    /// Delete `key`. A subsequent [`get`](Lsm::get) returns `None`.
    ///
    /// Deleting a key that is not present is not an error.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidInput`] if `key` is longer than
    /// [`MAX_KEY_LEN`](crate::format::MAX_KEY_LEN); nothing is written.
    /// [`Error::Io`] if logging the delete (with `durability`) or a resulting
    /// flush fails.
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
        self.engine.delete(key.as_ref())
    }

    /// Apply a [`Batch`] of writes as one group.
    ///
    /// The whole batch is applied under a single lock acquisition, so concurrent
    /// readers observe either none or all of it.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidInput`] if any key or value in the batch exceeds
    /// [`MAX_KEY_LEN`](crate::format::MAX_KEY_LEN) /
    /// [`MAX_VALUE_LEN`](crate::format::MAX_VALUE_LEN); none of the batch is
    /// written. [`Error::Io`] if logging the batch (with `durability`) or a
    /// resulting flush fails.
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
    /// # Ok(())
    /// # }
    /// ```
    pub fn write(&self, batch: Batch) -> Result<()> {
        self.engine.write(batch)
    }

    /// Look up `key`, returning its value, or `None` if it is absent or deleted.
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
        self.engine.get(key.as_ref())
    }

    /// Iterate the live `(key, value)` pairs whose key falls in `range`, in
    /// ascending key order.
    ///
    /// The range is taken over `Vec<u8>` bounds, so the usual syntaxes all work:
    /// `..`, `a..b`, `a..=b`, `a..`, `..b`. The returned [`Scan`] is a consistent
    /// snapshot taken when `scan` is called; later writes do not affect it.
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
    /// let mid: Vec<_> = db.scan(b"a".to_vec()..b"c".to_vec())?.collect();
    /// assert_eq!(mid, vec![(b"a".to_vec(), b"1".to_vec()), (b"b".to_vec(), b"2".to_vec())]);
    /// assert_eq!(db.scan(..)?.count(), 3);
    /// # Ok(())
    /// # }
    /// ```
    pub fn scan<R>(&self, range: R) -> Result<Scan>
    where
        R: RangeBounds<Vec<u8>>,
    {
        self.engine.scan(range)
    }

    /// Force the in-memory buffer to disk as a new run.
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
    /// drop(db);
    /// let db = lsm_db::Lsm::open(dir.path())?;
    /// assert_eq!(db.get(b"k")?, Some(b"v".to_vec()));
    /// # Ok(())
    /// # }
    /// ```
    pub fn flush(&self) -> Result<()> {
        self.engine.flush()
    }

    /// Every key that starts with `prefix`, with its value, in ascending key
    /// order: the scan of `prefix..` up to the first key that does not start
    /// with it. A prefix of only `0xFF` bytes (or an empty one) has no upper
    /// key, so its scan runs to the end of the key space.
    ///
    /// # Examples
    ///
    /// ```
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// let dir = tempfile::tempdir()?;
    /// let db = lsm_db::Lsm::open(dir.path())?;
    /// db.put(b"user:1", b"ann")?;
    /// db.put(b"user:2", b"bo")?;
    /// db.put(b"users", b"not a user key")?;
    /// let users: Vec<_> = db.scan_prefix(b"user:")?.map(|(k, _)| k).collect();
    /// assert_eq!(users, vec![b"user:1".to_vec(), b"user:2".to_vec()]);
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// # Errors
    ///
    /// As [`scan`](Self::scan).
    pub fn scan_prefix(&self, prefix: &[u8]) -> Result<Scan> {
        self.engine
            .scan((Bound::Included(prefix.to_vec()), prefix_end(prefix)))
    }

    /// Close the database: stop the background compactor and flush the
    /// in-memory buffer, returning any error the flush hits.
    ///
    /// Dropping the handle does the same, but has nowhere to report an
    /// error, so call `close` when the caller can act on one (a full disk,
    /// a failed sync). After `close` returns `Ok`, every write is in a run
    /// on disk and is read back on reopen.
    ///
    /// # Examples
    ///
    /// ```
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// let dir = tempfile::tempdir()?;
    /// let db = lsm_db::Lsm::open(dir.path())?;
    /// db.put(b"k", b"v")?;
    /// db.close()?;
    ///
    /// let db = lsm_db::Lsm::open(dir.path())?;
    /// assert_eq!(db.get(b"k")?, Some(b"v".to_vec()));
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// # Errors
    ///
    /// As [`flush`](Self::flush).
    pub fn close(mut self) -> Result<()> {
        self.shut_down()
    }

    /// Stop the compactor, then flush the buffer. Safe to call twice: the
    /// second call finds no compactor and an empty buffer.
    fn shut_down(&mut self) -> Result<()> {
        {
            let mut state = self
                .engine
                .compaction
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            state.shutdown = true;
        }
        self.engine.cond.notify_all();
        if let Some(handle) = self.compactor.take() {
            let _ = handle.join();
        }
        self.engine.flush()
    }

    /// Run one compaction synchronously. Test-only; production compaction is the
    /// background thread.
    #[cfg(test)]
    pub(crate) fn compact_now(&self) -> Result<()> {
        self.engine.compact_once()
    }

    /// The number of live on-disk runs. Test-only.
    #[cfg(test)]
    pub(crate) fn run_count(&self) -> usize {
        self.engine.read_guard().runs.len()
    }

    /// Block until the background compactor is idle (nothing pending or running).
    /// Test-only.
    #[cfg(test)]
    pub(crate) fn wait_for_idle(&self) {
        let mut state = self
            .engine
            .compaction
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        while state.pending || state.running {
            state = self
                .engine
                .cond
                .wait(state)
                .unwrap_or_else(|p| p.into_inner());
        }
    }
}

/// Dropping the handle stops the compactor and flushes the buffer, so a
/// write is not lost because nobody called [`Lsm::flush`]. An error from
/// that flush cannot be reported here; [`Lsm::close`] returns it.
impl Drop for Lsm {
    fn drop(&mut self) {
        let _ = self.shut_down();
    }
}

impl Engine {
    fn put(&self, key: &[u8], value: &[u8]) -> Result<()> {
        // Reject what the run format cannot store before anything is logged
        // or buffered; otherwise the write would succeed and the flushed run
        // would later read back as corrupt.
        format::check_lengths(key, format::Entry::Value(value))?;
        let record = Record::Value(value.to_vec());
        let mut inner = self.write_guard();
        // Durably log the write before it is acknowledged, then buffer it.
        inner.durability.log_one(key, &record)?;
        inner.memtable.apply(key.to_vec(), record);
        self.maybe_flush(&mut inner)
    }

    fn delete(&self, key: &[u8]) -> Result<()> {
        format::check_lengths(key, format::Entry::Tombstone)?;
        let record = Record::Tombstone;
        let mut inner = self.write_guard();
        inner.durability.log_one(key, &record)?;
        inner.memtable.apply(key.to_vec(), record);
        self.maybe_flush(&mut inner)
    }

    fn write(&self, batch: Batch) -> Result<()> {
        let ops: Vec<(Vec<u8>, Record)> = batch
            .into_ops()
            .into_iter()
            .map(|(key, op)| {
                let record = match op {
                    Op::Put(value) => Record::Value(value),
                    Op::Delete => Record::Tombstone,
                };
                (key, record)
            })
            .collect();
        for (key, record) in &ops {
            format::check_lengths(key, record.as_entry())?;
        }

        let mut inner = self.write_guard();
        // The whole batch is logged as one record, so it is recovered atomically.
        inner.durability.log_batch(&ops)?;
        for (key, record) in ops {
            inner.memtable.apply(key, record);
        }
        self.maybe_flush(&mut inner)
    }

    fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        let runs = {
            let inner = self.read_guard();
            match inner.memtable.get(key) {
                Some(Record::Value(value)) => return Ok(Some(value.clone())),
                Some(Record::Tombstone) => return Ok(None),
                None => inner.runs.clone(),
            }
        };
        // Runs are searched newest first, with no lock held. The bloom filter
        // lets a definite miss skip the run without reading any block.
        for run in &runs {
            if !run.might_contain(key) {
                continue;
            }
            match run.lookup(key)? {
                Some(Record::Value(value)) => return Ok(Some(value)),
                Some(Record::Tombstone) => return Ok(None),
                None => {}
            }
        }
        Ok(None)
    }

    fn scan<R>(&self, range: R) -> Result<Scan>
    where
        R: RangeBounds<Vec<u8>>,
    {
        let start = range.start_bound().map(Vec::as_slice);
        let end = range.end_bound().map(Vec::as_slice);
        let (mem, runs) = {
            let inner = self.read_guard();
            let mem: Vec<(Vec<u8>, Record)> = inner
                .memtable
                .range(start, end)
                .map(|(k, r)| (k.clone(), r.clone()))
                .collect();
            (mem, inner.runs.clone())
        };

        // Each run starts at the block that can hold the range's start, so
        // a scan costs its range, not every key before it.
        let cursors = runs.iter().map(|r| r.cursor_from(start)).collect();
        let mut out = Vec::new();
        for item in Merge::new(mem, cursors) {
            let (key, value) = item?;
            match position(&range, &key) {
                Pos::Below => {}
                Pos::In => out.push((key, value)),
                Pos::Above => break, // ascending stream: nothing further qualifies
            }
        }
        Ok(Scan::new(out))
    }

    fn flush(&self) -> Result<()> {
        let mut inner = self.write_guard();
        if inner.memtable.is_empty() {
            return Ok(());
        }
        self.flush_locked(&mut inner)
    }

    /// Flush if the buffer has reached the configured capacity.
    fn maybe_flush(&self, inner: &mut Inner) -> Result<()> {
        if !inner.memtable.is_empty()
            && inner.memtable.approx_size() >= self.config.memtable_capacity_bytes()
        {
            self.flush_locked(inner)?;
        }
        Ok(())
    }

    /// Write the memtable to a new run and install it, newest first.
    fn flush_locked(&self, inner: &mut Inner) -> Result<()> {
        let seq = self.next_seq.fetch_add(1, Ordering::SeqCst);
        let name = manifest::run_filename(seq);
        let tmp = self.dir.join(format!("{name}.tmp"));
        let final_path = self.dir.join(&name);
        // Until the manifest names the run, every file this flush makes is
        // removed on an error.
        let mut cleanup = Cleanup::new([
            tmp.clone(),
            final_path.clone(),
            bloom::sidecar_path(&final_path),
            bloom::sidecar_tmp_path(&final_path),
        ]);

        // The memtable stays as it is until the manifest names the new run:
        // a flush that fails anywhere before leaves every write readable,
        // and the next flush tries again.
        let table = self.write_run(&inner.memtable, &tmp, &final_path)?;
        let run = Arc::new(table);
        let mut new_runs = Vec::with_capacity(inner.runs.len() + 1);
        new_runs.push(run);
        new_runs.extend(inner.runs.iter().cloned());
        let names: Vec<String> = new_runs.iter().map(|r| r.file_name()).collect();
        Manifest::install(&self.dir, self.next_seq.load(Ordering::SeqCst), &names)?;
        // The manifest on disk names the run now, so its files stay whatever
        // happens next. If the directory sync fails the flush still fails and
        // the memtable is kept: the next flush writes it again and its
        // manifest leaves this run out, so the run is an orphan that the next
        // open removes. A crash before that finds either manifest, each
        // consistent with the files on disk.
        cleanup.disarm();
        crate::fault::check("sync manifest directory")?;
        crate::fault::sync_dir(&self.dir)?;
        inner.runs = new_runs;
        let _ = inner.memtable.take();

        // The flushed writes are now durable in the run, so the log that held
        // them can be emptied; recovery replays only writes since this flush.
        inner.durability.rotate()?;

        if inner.runs.len() >= self.config.compaction_trigger_runs() {
            self.signal_compaction();
        }
        Ok(())
    }

    /// Write `memtable` as the run at `final_path` (through `tmp`), with its
    /// bloom sidecar, and open it: everything a flush does before the
    /// manifest commit.
    fn write_run(&self, memtable: &MemTable, tmp: &Path, final_path: &Path) -> Result<SsTable> {
        let mut writer = SsTableWriter::create(tmp)?;
        let mut filter = bloom::builder(memtable.len());
        for (key, record) in memtable.iter() {
            writer.push(key, record)?;
            filter.add(key);
        }
        crate::fault::check("write flushed run")?;
        writer.finish()?;
        crate::fault::check("install flushed run")?;
        fs::rename(tmp, final_path).map_err(|e| Error::io("install flushed run", e))?;

        // Write the bloom sidecar before the manifest commit. It is not
        // synced: a sidecar lost or torn by a power loss fails its checksum
        // and the run is read without a filter.
        let filter = filter.finish();
        if let Some(filter) = &filter {
            filter.write_sidecar(final_path)?;
        }
        // The run's name (and its sidecar's) must be durable before the
        // manifest names it.
        crate::fault::sync_dir(&self.dir)?;
        let mut table = SsTable::open(final_path)?;
        table.attach_filter(filter);
        table.attach_cache(Arc::clone(&self.cache));
        Ok(table)
    }

    /// Merge every current run into a single new run, then swap it in.
    ///
    /// The merge runs with no lock held; only the final swap and manifest write
    /// take the write lock. Concurrency safety rests on the snapshot taken up
    /// front and on reference-counted run files (see module docs).
    fn compact_once(&self) -> Result<()> {
        if self.compacting.swap(true, Ordering::AcqRel) {
            return Ok(()); // another compaction is already running
        }
        // Cleared however the compaction ends, a panic included.
        struct Running<'a>(&'a AtomicBool);
        impl Drop for Running<'_> {
            fn drop(&mut self) {
                self.0.store(false, Ordering::Release);
            }
        }
        let _running = Running(&self.compacting);
        self.compact_inner()
    }

    fn compact_inner(&self) -> Result<()> {
        // Snapshot the runs to merge. Anything flushed after this stays newer
        // than the output and is preserved by the swap.
        let inputs: Vec<Arc<SsTable>> = {
            let inner = self.read_guard();
            if inner.runs.len() < 2 {
                return Ok(());
            }
            inner.runs.clone()
        };

        // Size the output filter from the sum of input entry counts: an upper
        // bound (dedup only lowers the real count), so the filter is never
        // under-sized. Each count was checked against its run's size at open,
        // so the sum is bounded by the bytes the inputs hold.
        let capacity: usize = inputs
            .iter()
            .map(|r| usize::try_from(r.entry_count()).unwrap_or(usize::MAX))
            .fold(0usize, |acc, n| acc.saturating_add(n));

        // Merge into a new run with no lock held.
        let seq = self.next_seq.fetch_add(1, Ordering::SeqCst);
        let name = manifest::run_filename(seq);
        let tmp = self.dir.join(format!("{name}.tmp"));
        let final_path = self.dir.join(&name);
        // Every file this compaction makes is removed unless the manifest
        // names the output, on an error and on a panic alike.
        let mut cleanup = Cleanup::new([
            tmp.clone(),
            final_path.clone(),
            bloom::sidecar_path(&final_path),
            bloom::sidecar_tmp_path(&final_path),
        ]);
        let mut filter = bloom::builder(capacity);
        {
            let mut writer = SsTableWriter::create(&tmp)?;
            let cursors = inputs.iter().map(|r| r.cursor()).collect();
            // Merging every run, so this output is the only level — tombstones
            // have nothing left to mask and are dropped.
            for item in Merge::new(Vec::new(), cursors) {
                let (key, value) = item?;
                writer.push(&key, &Record::Value(value))?;
                filter.add(&key);
            }
            #[cfg(test)]
            if self.inject_panic.swap(false, Ordering::SeqCst) {
                panic!("injected compaction panic");
            }
            crate::fault::check("write compacted run")?;
            writer.finish()?;
        }
        crate::fault::check("install compacted run")?;
        fs::rename(&tmp, &final_path).map_err(|e| Error::io("install compacted run", e))?;

        let filter = filter.finish();
        if let Some(filter) = &filter {
            filter.write_sidecar(&final_path)?;
        }
        crate::fault::sync_dir(&self.dir)?;
        let mut output = SsTable::open(&final_path)?;
        output.attach_filter(filter);
        output.attach_cache(Arc::clone(&self.cache));
        let output = Arc::new(output);

        // Swap: drop the inputs, keep any runs flushed during the merge, append
        // the output as the oldest run.
        {
            let mut inner = self.write_guard();
            let mut new_runs: Vec<Arc<SsTable>> = inner
                .runs
                .iter()
                .filter(|r| !inputs.iter().any(|i| Arc::ptr_eq(i, r)))
                .cloned()
                .collect();
            new_runs.push(Arc::clone(&output));

            let names: Vec<String> = new_runs.iter().map(|r| r.file_name()).collect();
            // The manifest rename is the commit point. If it fails the output
            // is removed and the live set is unchanged.
            Manifest::install(&self.dir, self.next_seq.load(Ordering::SeqCst), &names)?;
            // From here the manifest on disk names the output, so it stays.
            // A failed directory sync leaves the live set unchanged in memory:
            // the inputs stay live, and the next manifest written names them
            // again, which leaves the output an orphan for the next open. A
            // crash before that finds either manifest, each one complete.
            cleanup.disarm();
            crate::fault::check("sync manifest directory")?;
            crate::fault::sync_dir(&self.dir)?;
            for input in &inputs {
                input.mark_obsolete();
            }
            inner.runs = new_runs;
        }
        // Drop the snapshot; obsolete files are removed once no reader holds them.
        drop(inputs);
        Ok(())
    }

    /// Ask the background compactor to run.
    fn signal_compaction(&self) {
        let mut state = self.compaction.lock().unwrap_or_else(|p| p.into_inner());
        state.pending = true;
        self.cond.notify_all();
    }

    fn read_guard(&self) -> RwLockReadGuard<'_, Inner> {
        self.inner.read().unwrap_or_else(|p| p.into_inner())
    }

    fn write_guard(&self) -> RwLockWriteGuard<'_, Inner> {
        self.inner.write().unwrap_or_else(|p| p.into_inner())
    }
}

/// Files a flush or compaction made, removed on drop unless the manifest
/// came to name them.
struct Cleanup {
    paths: [PathBuf; 4],
    armed: bool,
}

impl Cleanup {
    fn new(paths: [PathBuf; 4]) -> Self {
        Cleanup { paths, armed: true }
    }

    /// Keep the files: the manifest names them.
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for Cleanup {
    fn drop(&mut self) {
        if self.armed {
            // Best effort: what stays behind is an orphan the next open
            // removes.
            for path in &self.paths {
                let _ = fs::remove_file(path);
            }
        }
    }
}

/// The background compaction loop: wait for work, compact, repeat until shutdown.
fn compactor_loop(engine: &Engine) {
    loop {
        {
            let mut state = engine.compaction.lock().unwrap_or_else(|p| p.into_inner());
            while !state.pending && !state.shutdown {
                state = engine.cond.wait(state).unwrap_or_else(|p| p.into_inner());
            }
            if state.shutdown {
                return;
            }
            state.pending = false;
            state.running = true;
        }

        // A panic inside the merge (a bug, or a run crafted to cause one)
        // ends this attempt, not the thread: it is reported like an error
        // and the next trigger tries again.
        let result =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| engine.compact_once()))
                .unwrap_or_else(|_| {
                    Err(Error::io(
                        "run background compaction",
                        std::io::Error::other("the compaction panicked"),
                    ))
                });

        {
            let mut state = engine.compaction.lock().unwrap_or_else(|p| p.into_inner());
            state.running = false;
            state.generation += 1;
            if let Err(err) = result {
                *engine.last_error.lock().unwrap_or_else(|p| p.into_inner()) = Some(err);
            }
            engine.cond.notify_all();
        }
    }
}

// Where a key sits relative to a range.
enum Pos {
    Below,
    In,
    Above,
}

/// The exclusive end of the keys that start with `prefix`: the prefix with
/// its last byte below `0xFF` raised by one and the bytes after it dropped.
/// A prefix of only `0xFF` bytes, or an empty one, has no such key: every key
/// after it starts with it, so the end is unbounded.
fn prefix_end(prefix: &[u8]) -> Bound<Vec<u8>> {
    match prefix.iter().rposition(|&b| b < u8::MAX) {
        Some(last) => {
            let mut end = prefix[..=last].to_vec();
            end[last] += 1;
            Bound::Excluded(end)
        }
        None => Bound::Unbounded,
    }
}

/// Classify `key` against `range`.
fn position<R: RangeBounds<Vec<u8>>>(range: &R, key: &[u8]) -> Pos {
    let below = match range.start_bound() {
        Bound::Included(s) => key < s.as_slice(),
        Bound::Excluded(s) => key <= s.as_slice(),
        Bound::Unbounded => false,
    };
    if below {
        return Pos::Below;
    }
    let above = match range.end_bound() {
        Bound::Included(e) => key > e.as_slice(),
        Bound::Excluded(e) => key >= e.as_slice(),
        Bound::Unbounded => false,
    };
    if above { Pos::Above } else { Pos::In }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    /// Open with compaction effectively disabled, for deterministic tests.
    fn db_no_autocompact() -> (tempfile::TempDir, Lsm) {
        let dir = tempfile::tempdir().unwrap();
        let db =
            Lsm::open_with(dir.path(), LsmConfig::new().compaction_trigger(usize::MAX)).unwrap();
        (dir, db)
    }

    fn db() -> (tempfile::TempDir, Lsm) {
        let dir = tempfile::tempdir().unwrap();
        let db = Lsm::open(dir.path()).unwrap();
        (dir, db)
    }

    /// Every step of a flush before the manifest commit, failed once: the
    /// flush returns the error, every write stays readable from the live
    /// handle, the next flush succeeds, and a reopen finds the data. Each
    /// point is armed on this thread only, so the failure is deterministic.
    #[test]
    fn test_a_failed_flush_keeps_the_memtable() {
        for point in [
            "write flushed run",
            "install flushed run",
            "sync directory",
            "install manifest",
        ] {
            let (dir, db) = db_no_autocompact();
            db.put(b"before", b"1").unwrap();
            db.flush().unwrap();
            db.put(b"a", b"2").unwrap();
            db.delete(b"before").unwrap();
            crate::fault::arm(point);
            assert!(db.flush().is_err(), "{point}: the flush should fail");
            assert_eq!(db.get(b"a").unwrap(), Some(b"2".to_vec()), "{point}");
            assert_eq!(db.get(b"before").unwrap(), None, "{point}");
            assert_eq!(db.run_count(), 1, "{point}: no run installed");
            // No file left that a later open would read as a run.
            let leftovers: Vec<String> = fs::read_dir(dir.path())
                .unwrap()
                .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                .filter(|n| n.ends_with(".tmp") || manifest::seq_of(n) == Some(1))
                .collect();
            assert!(leftovers.is_empty(), "{point}: {leftovers:?}");
            db.flush().unwrap();
            assert_eq!(db.run_count(), 2);
            drop(db);
            let db = Lsm::open(dir.path()).unwrap();
            assert_eq!(db.get(b"a").unwrap(), Some(b"2".to_vec()), "{point}");
            assert_eq!(db.get(b"before").unwrap(), None, "{point}");
        }
    }

    /// The directory is synced after a flush installs its run and again
    /// after the manifest names it, and after orphans are removed at open
    /// (a no-op sync on Windows, counted all the same).
    #[test]
    fn test_flushes_and_opens_sync_the_directory() {
        let (dir, db) = db_no_autocompact();
        db.put(b"k", b"v").unwrap();
        let before = crate::fault::dir_syncs();
        db.flush().unwrap();
        assert_eq!(crate::fault::dir_syncs() - before, 2);
        drop(db);
        fs::write(dir.path().join("run-0000000099.sst.tmp"), b"half a run").unwrap();
        let before = crate::fault::dir_syncs();
        let _db = Lsm::open(dir.path()).unwrap();
        assert!(crate::fault::dir_syncs() > before);
        assert!(!dir.path().join("run-0000000099.sst.tmp").exists());
    }

    /// A store writes its manifest when it is created; afterwards run files
    /// without a manifest refuse to open and are left as they are.
    #[test]
    fn test_a_missing_manifest_with_runs_refuses_to_open() {
        let dir = tempfile::tempdir().unwrap();
        drop(Lsm::open(dir.path()).unwrap());
        assert!(dir.path().join("MANIFEST").exists());
        let db = Lsm::open(dir.path()).unwrap();
        db.put(b"k", b"v").unwrap();
        db.flush().unwrap();
        drop(db);
        fs::remove_file(dir.path().join("MANIFEST")).unwrap();
        let runs = || {
            fs::read_dir(dir.path())
                .unwrap()
                .filter(|e| {
                    manifest::seq_of(&e.as_ref().unwrap().file_name().to_string_lossy()).is_some()
                })
                .count()
        };
        assert_eq!(runs(), 1);
        assert!(matches!(
            Lsm::open(dir.path()),
            Err(Error::Corruption { .. })
        ));
        assert_eq!(runs(), 1, "the run must not be deleted");
    }

    /// The names of the files in `dir`.
    fn file_names(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    /// Every run the manifest on disk names exists.
    fn manifest_runs_exist(dir: &Path) -> bool {
        Manifest::load(dir)
            .unwrap()
            .unwrap()
            .runs
            .iter()
            .all(|r| dir.join(r).exists())
    }

    /// A compaction that fails before its commit removes every file it made
    /// and leaves the live set as it was; the next one succeeds.
    #[test]
    fn test_a_failed_compaction_removes_its_files() {
        for point in ["write compacted run", "install compacted run"] {
            let (dir, db) = db_no_autocompact();
            for i in 0..3u32 {
                db.put(format!("k{i}"), b"v").unwrap();
                db.flush().unwrap();
            }
            let before = file_names(dir.path());
            crate::fault::arm(point);
            assert!(db.compact_now().is_err(), "{point}");
            assert_eq!(file_names(dir.path()), before, "{point}");
            assert_eq!(db.run_count(), 3);
            assert_eq!(db.scan(..).unwrap().count(), 3);
            db.compact_now().unwrap();
            assert_eq!(db.run_count(), 1);
        }
    }

    /// A directory sync that fails after the new manifest is in place: the
    /// operation fails, but no file the manifest on disk names is removed,
    /// and the live set in memory is unchanged, so the next manifest
    /// written agrees with it again. Before 1.4 a flush removed the run the
    /// new manifest named, and a crash then left a store that would not
    /// open.
    #[test]
    fn test_a_failed_sync_after_the_manifest_keeps_what_it_names() {
        let (dir, db) = db_no_autocompact();
        db.put(b"a", b"1").unwrap();
        db.flush().unwrap();
        db.put(b"b", b"2").unwrap();
        crate::fault::arm("sync manifest directory");
        assert!(db.flush().is_err());
        assert!(manifest_runs_exist(dir.path()));
        assert_eq!(db.run_count(), 1);
        assert_eq!(db.get(b"b").unwrap(), Some(b"2".to_vec()));

        db.flush().unwrap();
        assert!(manifest_runs_exist(dir.path()));
        crate::fault::arm("sync manifest directory");
        assert!(db.compact_now().is_err());
        assert!(manifest_runs_exist(dir.path()));
        assert_eq!(db.run_count(), 2);
        db.compact_now().unwrap();
        drop(db);
        let db = Lsm::open(dir.path()).unwrap();
        assert_eq!(db.get(b"a").unwrap(), Some(b"1".to_vec()));
        assert_eq!(db.get(b"b").unwrap(), Some(b"2".to_vec()));
    }

    /// A panic inside a background compaction is caught and reported; the
    /// compactor goes on, and the next compaction succeeds. Before 1.4 the
    /// thread died and no compaction ran again for the handle's life.
    #[test]
    fn test_a_compaction_panic_is_reported_and_the_compactor_survives() {
        let dir = tempfile::tempdir().unwrap();
        let db = Lsm::open_with(dir.path(), LsmConfig::new().compaction_trigger(2)).unwrap();
        db.engine.inject_panic.store(true, Ordering::SeqCst);
        for i in 0..2u32 {
            db.put(format!("k{i}"), b"v").unwrap();
            db.flush().unwrap();
        }
        db.wait_for_idle();
        let err = db.take_compaction_error().expect("the panic is reported");
        assert!(err.to_string().contains("panicked"), "{err}");
        assert!(!db.engine.compacting.load(Ordering::Acquire));
        assert_eq!(db.run_count(), 2);
        assert!(file_names(dir.path()).iter().all(|n| !n.ends_with(".tmp")));

        db.put(b"k2", b"v").unwrap();
        db.flush().unwrap();
        db.wait_for_idle();
        assert!(db.take_compaction_error().is_none());
        assert_eq!(db.run_count(), 1);
        assert_eq!(db.scan(..).unwrap().count(), 3);
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
    fn test_overwrite_across_runs() {
        let (_d, db) = db_no_autocompact();
        db.put(b"k", b"old").unwrap();
        db.flush().unwrap();
        db.put(b"k", b"new").unwrap();
        db.flush().unwrap();
        assert_eq!(db.run_count(), 2);
        assert_eq!(db.get(b"k").unwrap(), Some(b"new".to_vec()));
    }

    #[test]
    fn test_delete_masks_value_across_runs() {
        let (_d, db) = db_no_autocompact();
        db.put(b"k", b"v").unwrap();
        db.flush().unwrap();
        db.delete(b"k").unwrap();
        db.flush().unwrap();
        assert_eq!(db.get(b"k").unwrap(), None);
    }

    #[test]
    fn test_compaction_merges_to_single_run() {
        let (_d, db) = db_no_autocompact();
        for i in 0..5u32 {
            db.put(format!("k{i}").into_bytes(), format!("v{i}").into_bytes())
                .unwrap();
            db.flush().unwrap();
        }
        assert_eq!(db.run_count(), 5);
        db.compact_now().unwrap();
        assert_eq!(db.run_count(), 1);
        for i in 0..5u32 {
            assert_eq!(
                db.get(format!("k{i}").into_bytes()).unwrap(),
                Some(format!("v{i}").into_bytes())
            );
        }
    }

    #[test]
    fn test_compaction_drops_tombstones_and_keeps_latest() {
        let (_d, db) = db_no_autocompact();
        db.put(b"keep", b"1").unwrap();
        db.put(b"gone", b"x").unwrap();
        db.flush().unwrap();
        db.put(b"keep", b"2").unwrap(); // newer value
        db.delete(b"gone").unwrap(); // tombstone
        db.flush().unwrap();
        db.compact_now().unwrap();

        assert_eq!(db.run_count(), 1);
        assert_eq!(db.get(b"keep").unwrap(), Some(b"2".to_vec()));
        assert_eq!(db.get(b"gone").unwrap(), None);
        // The compacted run holds exactly one live entry.
        assert_eq!(db.scan(..).unwrap().count(), 1);
    }

    #[test]
    fn test_reopen_reads_all_runs() {
        let dir = tempfile::tempdir().unwrap();
        {
            let db = Lsm::open_with(dir.path(), LsmConfig::new().compaction_trigger(usize::MAX))
                .unwrap();
            db.put(b"a", b"1").unwrap();
            db.flush().unwrap();
            db.put(b"b", b"2").unwrap();
            db.flush().unwrap();
            db.put(b"a", b"updated").unwrap();
            db.flush().unwrap();
        }
        let db = Lsm::open(dir.path()).unwrap();
        assert_eq!(db.get(b"a").unwrap(), Some(b"updated".to_vec()));
        assert_eq!(db.get(b"b").unwrap(), Some(b"2".to_vec()));
    }

    #[test]
    fn test_reopen_after_compaction() {
        let dir = tempfile::tempdir().unwrap();
        {
            let db = Lsm::open_with(dir.path(), LsmConfig::new().compaction_trigger(usize::MAX))
                .unwrap();
            for i in 0..4u32 {
                db.put(format!("k{i}").into_bytes(), b"v").unwrap();
                db.flush().unwrap();
            }
            db.compact_now().unwrap();
            assert_eq!(db.run_count(), 1);
        }
        let db = Lsm::open(dir.path()).unwrap();
        assert_eq!(db.run_count(), 1);
        assert_eq!(db.scan(..).unwrap().count(), 4);
    }

    #[test]
    fn test_background_compaction_triggers() {
        let dir = tempfile::tempdir().unwrap();
        let db = Lsm::open_with(dir.path(), LsmConfig::new().compaction_trigger(3)).unwrap();
        for i in 0..10u32 {
            db.put(format!("k{i:02}").into_bytes(), b"v").unwrap();
            db.flush().unwrap();
        }
        db.wait_for_idle();
        // Compaction should have collapsed the runs well below the flush count.
        assert!(db.run_count() <= 3, "run count was {}", db.run_count());
        for i in 0..10u32 {
            assert_eq!(
                db.get(format!("k{i:02}").into_bytes()).unwrap(),
                Some(b"v".to_vec())
            );
        }
    }

    #[test]
    fn test_scan_merges_across_runs() {
        let (_d, db) = db_no_autocompact();
        db.put(b"a", b"old-a").unwrap();
        db.put(b"c", b"3").unwrap();
        db.flush().unwrap();
        db.put(b"a", b"new-a").unwrap();
        db.put(b"b", b"2").unwrap();
        db.delete(b"c").unwrap();
        db.flush().unwrap();
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
    fn test_scan_bounded_range() {
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
    fn test_empty_value_roundtrips_through_flush() {
        let (_d, db) = db_no_autocompact();
        db.put(b"k", b"").unwrap();
        db.flush().unwrap();
        assert_eq!(db.get(b"k").unwrap(), Some(Vec::new()));
        db.compact_now().unwrap();
        assert_eq!(db.get(b"k").unwrap(), Some(Vec::new()));
    }

    #[test]
    fn test_engine_is_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<Lsm>();
    }

    /// The bloom-filter contract (`bloom` feature): a negative point lookup
    /// reads no data blocks, because every run's filter rejects the absent key,
    /// while a positive lookup still reads a block. This is the deterministic,
    /// CI-enforced form of the 0.5 exit criterion.
    #[cfg(feature = "bloom")]
    #[test]
    fn test_bloom_skips_blocks_on_negative_lookup() {
        use crate::sstable::block_reads;

        let (_d, db) = db_no_autocompact();
        // Several runs, each covering an overlapping key range, so an absent key
        // would otherwise force one candidate-block read per run.
        for run in 0..6u32 {
            for i in 0..50u32 {
                let key = format!("k{:04}", i * 2); // even keys only
                db.put(key.as_bytes(), format!("r{run}").as_bytes())
                    .unwrap();
            }
            db.flush().unwrap();
        }
        assert_eq!(db.run_count(), 6);

        // Negative lookup for an odd key that sorts *inside* every run's range.
        block_reads::reset();
        assert_eq!(db.get(b"k0051").unwrap(), None);
        assert_eq!(
            block_reads::count(),
            0,
            "bloom filters must let a negative lookup skip every run with no block read"
        );

        // A positive lookup does read a block (the counter is wired correctly).
        block_reads::reset();
        assert!(db.get(b"k0010").unwrap().is_some());
        assert!(
            block_reads::count() >= 1,
            "a hit must read at least one block"
        );
    }

    /// Compaction installs a sidecar for its output and removes the obsoleted
    /// inputs' sidecars, leaving exactly one sidecar per live run.
    #[cfg(feature = "bloom")]
    #[test]
    fn test_bloom_sidecars_track_runs_through_compaction() {
        let count = |dir: &std::path::Path, suffix: &str| {
            std::fs::read_dir(dir)
                .unwrap()
                .filter(|e| {
                    e.as_ref()
                        .unwrap()
                        .file_name()
                        .to_string_lossy()
                        .ends_with(suffix)
                })
                .count()
        };

        let (dir, db) = db_no_autocompact();
        for i in 0..5u32 {
            db.put(format!("k{i}").into_bytes(), b"v").unwrap();
            db.flush().unwrap();
        }
        assert_eq!(count(dir.path(), ".sst.bloom"), 5);

        db.compact_now().unwrap();
        assert_eq!(db.run_count(), 1);
        // Exactly one run and one sidecar; the obsoleted inputs' sidecars are
        // gone (dropped alongside their runs).
        assert_eq!(count(dir.path(), ".sst"), 1);
        assert_eq!(count(dir.path(), ".sst.bloom"), 1);
        for i in 0..5u32 {
            assert_eq!(
                db.get(format!("k{i}").into_bytes()).unwrap(),
                Some(b"v".to_vec())
            );
        }
    }

    /// With the block cache on (the default), a repeat lookup of the same key
    /// serves its block from cache and reads no data block.
    #[cfg(feature = "bloom")]
    #[test]
    fn test_block_cache_serves_repeat_lookup() {
        use crate::sstable::block_reads;

        let (_d, db) = db(); // default config: 8 MiB block cache
        db.put(b"k", b"v").unwrap();
        db.flush().unwrap();

        block_reads::reset();
        assert_eq!(db.get(b"k").unwrap(), Some(b"v".to_vec()));
        assert!(block_reads::count() >= 1, "cold lookup reads its block");

        block_reads::reset();
        assert_eq!(db.get(b"k").unwrap(), Some(b"v".to_vec()));
        assert_eq!(
            block_reads::count(),
            0,
            "a repeat lookup must be served from the block cache"
        );
    }

    /// With the block cache disabled, every lookup reads its block.
    #[cfg(feature = "bloom")]
    #[test]
    fn test_block_cache_disabled_always_reads() {
        use crate::sstable::block_reads;

        let dir = tempfile::tempdir().unwrap();
        let db = Lsm::open_with(dir.path(), LsmConfig::new().block_cache_capacity(0)).unwrap();
        db.put(b"k", b"v").unwrap();
        db.flush().unwrap();

        for _ in 0..2 {
            block_reads::reset();
            assert_eq!(db.get(b"k").unwrap(), Some(b"v".to_vec()));
            assert!(
                block_reads::count() >= 1,
                "with the cache off, every lookup reads its block"
            );
        }
    }
}
