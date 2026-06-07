//! Loom model of the read/compaction concurrency protocol.
//!
//! This is compiled and run only under `RUSTFLAGS="--cfg loom"`; an ordinary
//! `cargo test` sees an empty file. It models the exact synchronization the
//! engine uses for reads versus compaction (see `src/db.rs`): engine state is an
//! `RwLock` over the ordered run list; a reader clones the run list under a read
//! lock and then resolves the key newest-first with no lock held; a compaction
//! snapshots the runs, merges them with no lock held, and swaps the result in
//! under the write lock, keeping live runs alive through reference-counted
//! handles. Loom exhaustively explores the thread interleavings and checks that a
//! reader never observes a torn or lost result.

#![cfg(loom)]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashMap;

use loom::sync::{Arc, RwLock};
use loom::thread;

/// In-memory stand-in for a run's record (no file I/O, so loom can model it).
#[derive(Clone)]
enum Record {
    Value(u64),
    Tombstone,
}

/// In-memory stand-in for an on-disk run.
struct Run {
    data: HashMap<Vec<u8>, Record>,
}

impl Run {
    fn one(key: &[u8], record: Record) -> Arc<Run> {
        let mut data = HashMap::new();
        data.insert(key.to_vec(), record);
        Arc::new(Run { data })
    }

    fn lookup(&self, key: &[u8]) -> Option<Record> {
        self.data.get(key).cloned()
    }
}

/// The engine's read resolution: newest run first, tombstone hides older values.
/// Mirrors `Engine::get` in `src/db.rs`.
fn version_get(runs: &[Arc<Run>], key: &[u8]) -> Option<u64> {
    for run in runs {
        match run.lookup(key) {
            Some(Record::Value(v)) => return Some(v),
            Some(Record::Tombstone) => return None,
            None => {}
        }
    }
    None
}

/// A reader running concurrently with a compaction always resolves the newest
/// value, whether it observes the run list before or after the swap.
#[test]
fn reader_sees_newest_value_through_compaction() {
    loom::model(|| {
        let key = b"k";
        let runs = Arc::new(RwLock::new(vec![
            Run::one(key, Record::Value(2)), // newest
            Run::one(key, Record::Value(1)), // oldest
        ]));

        let compactor = {
            let runs = runs.clone();
            thread::spawn(move || {
                // Snapshot under the read lock, merge with no lock held.
                let inputs = runs.read().unwrap().clone();
                let merged = version_get(&inputs, key);
                let output = match merged {
                    Some(v) => Run::one(key, Record::Value(v)),
                    None => Arc::new(Run {
                        data: HashMap::new(),
                    }),
                };
                // Swap the single merged run in under the write lock.
                *runs.write().unwrap() = vec![output];
            })
        };

        let reader = {
            let runs = runs.clone();
            thread::spawn(move || {
                let snapshot = runs.read().unwrap().clone();
                assert_eq!(version_get(&snapshot, key), Some(2));
            })
        };

        compactor.join().unwrap();
        reader.join().unwrap();
        assert_eq!(version_get(&runs.read().unwrap(), key), Some(2));
    });
}

/// The same, but the newest record is a tombstone: the reader must always
/// observe the key as deleted, and compaction must preserve that.
#[test]
fn reader_sees_deletion_through_compaction() {
    loom::model(|| {
        let key = b"k";
        let runs = Arc::new(RwLock::new(vec![
            Run::one(key, Record::Tombstone), // newest: deleted
            Run::one(key, Record::Value(1)),  // oldest
        ]));

        let compactor = {
            let runs = runs.clone();
            thread::spawn(move || {
                let inputs = runs.read().unwrap().clone();
                let merged = version_get(&inputs, key);
                // Full compaction drops tombstones: the merged value is None,
                // so the output run is empty.
                let output = match merged {
                    Some(v) => Run::one(key, Record::Value(v)),
                    None => Arc::new(Run {
                        data: HashMap::new(),
                    }),
                };
                *runs.write().unwrap() = vec![output];
            })
        };

        let reader = {
            let runs = runs.clone();
            thread::spawn(move || {
                let snapshot = runs.read().unwrap().clone();
                assert_eq!(version_get(&snapshot, key), None);
            })
        };

        compactor.join().unwrap();
        reader.join().unwrap();
        assert_eq!(version_get(&runs.read().unwrap(), key), None);
    });
}
