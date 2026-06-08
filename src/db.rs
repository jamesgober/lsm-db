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
//! and a memtable slice) under a brief read lock, then do their file I/O without
//! holding it. Writes take the write lock. Compaction does its expensive merge
//! with no lock held and takes the write lock only to swap the finished run in,
//! so it never blocks reads or writes for more than that swap. A run removed by
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
/// consistent run set. Writes still buffered in the memtable when the process
/// exits without a flush are **not** yet crash-safe; write-ahead logging arrives
/// under the `durability` feature in a later release. Call [`flush`](Lsm::flush)
/// to force the buffer to disk on demand.
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
}

impl Lsm {
    /// Open the database in `dir`, creating the directory if it does not exist,
    /// using the default [`LsmConfig`].
    ///
    /// The run set recorded in the manifest is reopened, so flushed data is
    /// visible immediately. Temporary files and run files orphaned by a crash
    /// mid-flush or mid-compaction are reclaimed.
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

        let manifest = Manifest::load(&dir)?;
        let (run_names, manifest_seq) = match manifest {
            Some(m) => (m.runs, m.next_seq),
            None => (Vec::new(), 0),
        };
        let live: HashSet<&str> = run_names.iter().map(String::as_str).collect();

        // Shared block cache for point lookups, sized from the config.
        let cache = BlockCache::new(config.block_cache_capacity_bytes());

        // Open the live runs in recency order, attaching each run's bloom filter
        // from its sidecar (a no-op without the `bloom` feature, and a tolerated
        // miss if a sidecar is absent — the run is simply consulted directly) and
        // the shared block cache.
        let mut runs = Vec::with_capacity(run_names.len());
        for name in &run_names {
            let path = dir.join(name);
            if !path.exists() {
                return Err(Error::corruption("manifest references a missing run"));
            }
            let mut table = SsTable::open(&path)?;
            table.attach_filter(RunFilter::load(&path)?);
            table.attach_cache(Arc::clone(&cache));
            runs.push(Arc::new(table));
        }

        // Reclaim orphans (temporaries and runs no longer in the manifest) and
        // make sure the next sequence number is past every file on disk.
        let mut next_seq = manifest_seq;
        for entry in fs::read_dir(&dir).map_err(|e| Error::io("scan database directory", e))? {
            let entry = entry.map_err(|e| Error::io("read directory entry", e))?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.ends_with(".tmp") {
                fs::remove_file(entry.path()).map_err(|e| Error::io("remove temporary file", e))?;
            } else if let Some(run) = name.strip_suffix(".bloom") {
                // A bloom sidecar whose run is not live is an orphan (e.g. from a
                // compaction that crashed before its manifest commit).
                if !live.contains(run) {
                    fs::remove_file(entry.path())
                        .map_err(|e| Error::io("remove orphan bloom sidecar", e))?;
                }
            } else if let Some(seq) = manifest::seq_of(&name) {
                next_seq = next_seq.max(seq + 1);
                if !live.contains(name.as_str()) {
                    fs::remove_file(entry.path()).map_err(|e| Error::io("remove orphan run", e))?;
                }
            }
        }

        // Open the write-ahead log and replay any writes not yet flushed into a
        // fresh memtable. With the `durability` feature off this is a no-op.
        let durability = Durability::open(&dir)?;
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
        })
    }

    /// Set `key` to `value`, overwriting any previous value.
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

impl Drop for Lsm {
    fn drop(&mut self) {
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
    }
}

impl Engine {
    fn put(&self, key: &[u8], value: &[u8]) -> Result<()> {
        let record = Record::Value(value.to_vec());
        let mut inner = self.write_guard();
        // Durably log the write before it is acknowledged, then buffer it.
        inner.durability.log_one(key, &record)?;
        inner.memtable.apply(key.to_vec(), record);
        self.maybe_flush(&mut inner)
    }

    fn delete(&self, key: &[u8]) -> Result<()> {
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
        let (mem, runs) = {
            let inner = self.read_guard();
            let mem: Vec<(Vec<u8>, Record)> = inner
                .memtable
                .iter()
                .filter(|(k, _)| matches!(position(&range, k), Pos::In))
                .map(|(k, r)| (k.clone(), r.clone()))
                .collect();
            (mem, inner.runs.clone())
        };

        let cursors = runs.iter().map(|r| r.cursor()).collect();
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
        let entries = inner.memtable.take();
        let seq = self.next_seq.fetch_add(1, Ordering::SeqCst);
        let name = manifest::run_filename(seq);
        let tmp = self.dir.join(format!("{name}.tmp"));
        let final_path = self.dir.join(&name);

        let mut writer = SsTableWriter::create(&tmp)?;
        let mut filter = bloom::builder(entries.len());
        for (key, record) in &entries {
            writer.push(key, record)?;
            filter.add(key);
        }
        writer.finish()?;
        fs::rename(&tmp, &final_path).map_err(|e| Error::io("install flushed run", e))?;

        // Write the bloom sidecar before the manifest commit, so any run the
        // manifest names is guaranteed to have its sidecar on disk.
        let filter = filter.finish();
        if let Some(filter) = &filter {
            filter.write_sidecar(&final_path)?;
        }
        let mut table = SsTable::open(&final_path)?;
        table.attach_filter(filter);
        table.attach_cache(Arc::clone(&self.cache));

        let run = Arc::new(table);
        let mut new_runs = Vec::with_capacity(inner.runs.len() + 1);
        new_runs.push(run);
        new_runs.extend(inner.runs.iter().cloned());

        let names: Vec<String> = new_runs.iter().map(|r| r.file_name()).collect();
        Manifest::store(&self.dir, self.next_seq.load(Ordering::SeqCst), &names)?;
        inner.runs = new_runs;

        // The flushed writes are now durable in the run, so the log that held
        // them can be emptied; recovery replays only writes since this flush.
        inner.durability.rotate()?;

        if inner.runs.len() >= self.config.compaction_trigger_runs() {
            self.signal_compaction();
        }
        Ok(())
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
        let result = self.compact_inner();
        self.compacting.store(false, Ordering::Release);
        result
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

        // Size the output filter from the sum of input entry counts — an upper
        // bound (dedup only lowers the real count), so the filter is never
        // under-sized.
        let capacity: usize = inputs
            .iter()
            .map(|r| usize::try_from(r.entry_count()).unwrap_or(usize::MAX))
            .fold(0usize, |acc, n| acc.saturating_add(n));

        // Merge into a new run with no lock held.
        let seq = self.next_seq.fetch_add(1, Ordering::SeqCst);
        let name = manifest::run_filename(seq);
        let tmp = self.dir.join(format!("{name}.tmp"));
        let final_path = self.dir.join(&name);
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
            writer.finish()?;
        }
        fs::rename(&tmp, &final_path).map_err(|e| Error::io("install compacted run", e))?;

        let filter = filter.finish();
        if let Some(filter) = &filter {
            filter.write_sidecar(&final_path)?;
        }
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
            // The manifest rename is the commit point. If it fails the output is
            // an orphan the next open reclaims, and the live set is unchanged.
            Manifest::store(&self.dir, self.next_seq.load(Ordering::SeqCst), &names)?;
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

        let result = engine.compact_once();

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

/// Where a key sits relative to a range.
enum Pos {
    Below,
    In,
    Above,
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
