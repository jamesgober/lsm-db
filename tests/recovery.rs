//! Crash recovery and on-disk integrity.
//!
//! The manifest is the source of truth for which runs are live. These tests
//! confirm that an ungraceful exit, a leftover temporary file from an interrupted
//! flush, and an orphan run from an interrupted compaction all recover to a
//! consistent state, and that a corrupted run is detected rather than silently
//! returning wrong data.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::path::{Path, PathBuf};

use lsm_db::{Error, Lsm, LsmConfig};

/// A config that never auto-compacts, so run files are predictable.
fn no_compact() -> LsmConfig {
    LsmConfig::new().compaction_trigger(usize::MAX)
}

/// Collect the run files (`run-*.sst`) currently in `dir`.
fn run_files(dir: &Path) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("run-") && n.ends_with(".sst"))
        })
        .collect();
    files.sort();
    files
}

#[test]
fn ungraceful_exit_preserves_flushed_data() {
    let dir = tempfile::tempdir().unwrap();
    {
        let db = Lsm::open_with(dir.path(), no_compact()).unwrap();
        db.put(b"a", b"1").unwrap();
        db.flush().unwrap();
        db.put(b"b", b"2").unwrap();
        db.flush().unwrap();
        // Simulate a crash: skip the graceful shutdown (no Drop, no thread join).
        std::mem::forget(db);
    }
    let db = Lsm::open(dir.path()).unwrap();
    assert_eq!(db.get(b"a").unwrap(), Some(b"1".to_vec()));
    assert_eq!(db.get(b"b").unwrap(), Some(b"2".to_vec()));
}

#[test]
fn stale_temp_file_is_removed_on_open() {
    let dir = tempfile::tempdir().unwrap();
    {
        let db = Lsm::open_with(dir.path(), no_compact()).unwrap();
        db.put(b"k", b"v").unwrap();
        db.flush().unwrap();
    }
    // Leftover from a flush interrupted before its atomic rename.
    let tmp = dir.path().join("run-0000009999.sst.tmp");
    fs::write(&tmp, b"partial garbage").unwrap();

    let db = Lsm::open(dir.path()).unwrap();
    assert!(!tmp.exists(), "temporary file should be reclaimed");
    assert_eq!(db.get(b"k").unwrap(), Some(b"v".to_vec()));
}

#[test]
fn orphan_run_is_removed_on_open() {
    let dir = tempfile::tempdir().unwrap();
    {
        let db = Lsm::open_with(dir.path(), no_compact()).unwrap();
        db.put(b"k", b"v").unwrap();
        db.flush().unwrap();
    }
    // Simulate a compaction that wrote its output then crashed before updating
    // the manifest: an extra valid run file not named by the manifest.
    let existing = run_files(dir.path());
    assert_eq!(existing.len(), 1);
    let orphan = dir.path().join("run-0000008888.sst");
    fs::copy(&existing[0], &orphan).unwrap();

    let db = Lsm::open(dir.path()).unwrap();
    assert!(!orphan.exists(), "orphan run should be reclaimed");
    assert_eq!(db.get(b"k").unwrap(), Some(b"v".to_vec()));
    // The live set is just the manifest's single run.
    assert_eq!(run_files(dir.path()).len(), 1);
}

#[test]
fn corrupted_run_is_detected() {
    let dir = tempfile::tempdir().unwrap();
    {
        let db = Lsm::open_with(dir.path(), no_compact()).unwrap();
        db.put(b"alpha", b"first").unwrap();
        db.put(b"beta", b"second").unwrap();
        db.flush().unwrap();
    }
    // Flip a byte inside the first data block (just past the 8-byte magic).
    let run = run_files(dir.path()).pop().unwrap();
    let mut bytes = fs::read(&run).unwrap();
    bytes[12] ^= 0xFF;
    fs::write(&run, &bytes).unwrap();

    // Open still succeeds (footer and index are intact)...
    let db = Lsm::open(dir.path()).unwrap();
    // ...but reading the damaged block surfaces a corruption error.
    assert!(matches!(db.get(b"alpha"), Err(Error::Corruption { .. })));
}

#[test]
fn missing_run_named_by_manifest_is_corruption() {
    let dir = tempfile::tempdir().unwrap();
    {
        let db = Lsm::open_with(dir.path(), no_compact()).unwrap();
        db.put(b"k", b"v").unwrap();
        db.flush().unwrap();
    }
    // Delete a run the manifest still references.
    let run = run_files(dir.path()).pop().unwrap();
    fs::remove_file(&run).unwrap();

    assert!(matches!(
        Lsm::open(dir.path()),
        Err(Error::Corruption { .. })
    ));
}

#[test]
fn many_flush_compact_cycles_reopen_clean() {
    let dir = tempfile::tempdir().unwrap();
    {
        let db = Lsm::open_with(
            dir.path(),
            LsmConfig::new()
                .memtable_capacity(256)
                .compaction_trigger(3),
        )
        .unwrap();
        for i in 0..2_000u32 {
            db.put(format!("key-{i:05}").into_bytes(), b"v").unwrap();
        }
        for i in 0..500u32 {
            db.delete(format!("key-{i:05}").into_bytes()).unwrap();
        }
        db.flush().unwrap();
        // Graceful shutdown joins the compactor.
    }
    let db = Lsm::open(dir.path()).unwrap();
    assert_eq!(db.scan(..).unwrap().count(), 1_500);
    assert_eq!(db.get(b"key-00000").unwrap(), None);
    assert_eq!(db.get(b"key-00500").unwrap(), Some(b"v".to_vec()));
    assert_eq!(db.get(b"key-01999").unwrap(), Some(b"v".to_vec()));
}
