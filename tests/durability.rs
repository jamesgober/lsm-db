//! Crash recovery under the `durability` feature.
//!
//! Compiled and run only when the feature is enabled (so under `--all-features`).
//! With durability on, a write is logged and `fsync`ed before it is
//! acknowledged, and the log is replayed on open. These tests confirm the
//! headline guarantee: an acknowledged write that was never flushed is still
//! present after the engine is dropped and reopened.
//!
//! Dropping an `Lsm` flushes its memtable, so these tests end a handle with
//! [`crash`] instead: it leaves the directory exactly as it was while the
//! handle was open, which is the unflushed-but-acknowledged state a crash
//! leaves, with the writes sitting in the durable log. Reopening must
//! recover them.

#![cfg(feature = "durability")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::path::Path;

use lsm_db::{Batch, Lsm, LsmConfig};

/// Copy every file in `from` to `to`, except the lock file (a crash leaves
/// it, but the lock with it is gone, and the next open takes it again).
fn copy_files(from: &Path, to: &Path) {
    for entry in fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        if entry.file_name() != "LOCK" {
            let _ = fs::copy(entry.path(), to.join(entry.file_name())).unwrap();
        }
    }
}

/// End a handle as a crash would: no flush, no compactor shutdown. The
/// handle's directory is copied aside while it is open, the handle is
/// dropped (which releases its directory lock), and the copy is put back.
fn crash(db: Lsm, dir: &Path) {
    let saved = tempfile::tempdir().unwrap();
    copy_files(dir, saved.path());
    drop(db);
    for entry in fs::read_dir(dir).unwrap() {
        fs::remove_file(entry.unwrap().path()).unwrap();
    }
    copy_files(saved.path(), dir);
}

/// A durable engine that never auto-compacts, for predictable run counts.
fn durable(dir: &std::path::Path) -> Lsm {
    Lsm::open_with(dir, LsmConfig::new().compaction_trigger(usize::MAX)).unwrap()
}

#[test]
fn unflushed_writes_are_recovered() {
    let dir = tempfile::tempdir().unwrap();
    {
        let db = durable(dir.path());
        db.put(b"a", b"1").unwrap();
        db.put(b"b", b"2").unwrap();
        // No flush: the writes live only in the log and the memtable.
        crash(db, dir.path());
    }
    let db = durable(dir.path());
    assert_eq!(db.get(b"a").unwrap(), Some(b"1".to_vec()));
    assert_eq!(db.get(b"b").unwrap(), Some(b"2".to_vec()));
}

#[test]
fn unflushed_overwrite_is_recovered() {
    let dir = tempfile::tempdir().unwrap();
    {
        let db = durable(dir.path());
        db.put(b"k", b"first").unwrap();
        db.flush().unwrap(); // first value goes to a run
        db.put(b"k", b"second").unwrap(); // overwrite stays only in the log
        crash(db, dir.path());
    }
    let db = durable(dir.path());
    assert_eq!(db.get(b"k").unwrap(), Some(b"second".to_vec()));
}

#[test]
fn unflushed_delete_is_recovered() {
    let dir = tempfile::tempdir().unwrap();
    {
        let db = durable(dir.path());
        db.put(b"k", b"v").unwrap();
        db.flush().unwrap();
        db.delete(b"k").unwrap(); // tombstone only in the log
        crash(db, dir.path());
    }
    let db = durable(dir.path());
    assert_eq!(db.get(b"k").unwrap(), None);
}

#[test]
fn unflushed_batch_is_recovered_atomically() {
    let dir = tempfile::tempdir().unwrap();
    {
        let db = durable(dir.path());
        let mut batch = Batch::new();
        for i in 0..200u32 {
            batch.put(format!("k{i:03}").into_bytes(), b"v");
        }
        db.write(batch).unwrap();
        crash(db, dir.path());
    }
    let db = durable(dir.path());
    assert_eq!(db.scan(..).unwrap().count(), 200);
    assert_eq!(db.get(b"k000").unwrap(), Some(b"v".to_vec()));
    assert_eq!(db.get(b"k199").unwrap(), Some(b"v".to_vec()));
}

#[test]
fn mixed_flushed_and_unflushed_recover() {
    let dir = tempfile::tempdir().unwrap();
    {
        let db = durable(dir.path());
        db.put(b"flushed", b"1").unwrap();
        db.flush().unwrap();
        db.put(b"buffered-a", b"2").unwrap();
        db.put(b"buffered-b", b"3").unwrap();
        crash(db, dir.path());
    }
    let db = durable(dir.path());
    assert_eq!(db.get(b"flushed").unwrap(), Some(b"1".to_vec()));
    assert_eq!(db.get(b"buffered-a").unwrap(), Some(b"2".to_vec()));
    assert_eq!(db.get(b"buffered-b").unwrap(), Some(b"3".to_vec()));
    assert_eq!(db.scan(..).unwrap().count(), 3);
}

#[test]
fn reopen_twice_does_not_duplicate() {
    let dir = tempfile::tempdir().unwrap();
    {
        let db = durable(dir.path());
        db.put(b"k", b"v").unwrap();
        crash(db, dir.path());
    }
    // First reopen replays the log and checkpoints it into a run.
    {
        let db = durable(dir.path());
        assert_eq!(db.get(b"k").unwrap(), Some(b"v".to_vec()));
        assert_eq!(db.scan(..).unwrap().count(), 1);
        crash(db, dir.path());
    }
    // Second reopen: the log was emptied at checkpoint, data is in the run.
    let db = durable(dir.path());
    assert_eq!(db.get(b"k").unwrap(), Some(b"v".to_vec()));
    assert_eq!(db.scan(..).unwrap().count(), 1);
}

#[test]
fn writes_continue_after_recovery() {
    let dir = tempfile::tempdir().unwrap();
    {
        let db = durable(dir.path());
        db.put(b"before", b"1").unwrap();
        crash(db, dir.path());
    }
    let db = durable(dir.path());
    db.put(b"after", b"2").unwrap();
    assert_eq!(db.get(b"before").unwrap(), Some(b"1".to_vec()));
    assert_eq!(db.get(b"after").unwrap(), Some(b"2".to_vec()));

    // And those new writes are themselves durable.
    crash(db, dir.path());
    let db = durable(dir.path());
    assert_eq!(db.get(b"after").unwrap(), Some(b"2".to_vec()));
}

#[test]
fn many_unflushed_writes_recover() {
    let dir = tempfile::tempdir().unwrap();
    {
        let db = durable(dir.path());
        for i in 0..1_000u32 {
            db.put(
                format!("key-{i:04}").into_bytes(),
                format!("val-{i}").into_bytes(),
            )
            .unwrap();
        }
        // Delete a slice, also unflushed.
        for i in 0..200u32 {
            db.delete(format!("key-{i:04}").into_bytes()).unwrap();
        }
        crash(db, dir.path());
    }
    let db = durable(dir.path());
    assert_eq!(db.scan(..).unwrap().count(), 800);
    assert_eq!(db.get(b"key-0000").unwrap(), None);
    assert_eq!(db.get(b"key-0199").unwrap(), None);
    assert_eq!(db.get(b"key-0200").unwrap(), Some(b"val-200".to_vec()));
    assert_eq!(db.get(b"key-0999").unwrap(), Some(b"val-999".to_vec()));
}
