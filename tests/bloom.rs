//! Bloom-filter behaviour at the public-API boundary (`bloom` feature).
//!
//! These confirm the sidecar lifecycle and, above all, that the filter is a
//! pure acceleration: results are identical whether or not a sidecar is present,
//! so a missing or stale sidecar can never change an answer.

#![cfg(feature = "bloom")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::path::{Path, PathBuf};

use lsm_db::{Lsm, LsmConfig};

fn no_compact() -> LsmConfig {
    LsmConfig::new().compaction_trigger(usize::MAX)
}

fn files_with(dir: &Path, suffix: &str) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("run-") && n.ends_with(suffix))
        })
        .collect();
    v.sort();
    v
}

#[test]
fn flush_writes_a_bloom_sidecar_per_run() {
    let dir = tempfile::tempdir().unwrap();
    let db = Lsm::open_with(dir.path(), no_compact()).unwrap();
    for run in 0..3u32 {
        db.put(format!("k{run}").into_bytes(), b"v").unwrap();
        db.flush().unwrap();
    }
    let runs = files_with(dir.path(), ".sst");
    let blooms = files_with(dir.path(), ".sst.bloom");
    assert_eq!(runs.len(), 3);
    assert_eq!(blooms.len(), 3, "each run should have a bloom sidecar");
}

#[test]
fn results_correct_with_bloom_and_across_reopen() {
    let dir = tempfile::tempdir().unwrap();
    {
        let db = Lsm::open_with(dir.path(), no_compact()).unwrap();
        for i in 0..500u32 {
            db.put(
                format!("key-{i:04}").into_bytes(),
                format!("v{i}").into_bytes(),
            )
            .unwrap();
            if i % 100 == 99 {
                db.flush().unwrap(); // five runs
            }
        }
        // present and absent lookups
        assert_eq!(db.get(b"key-0000").unwrap(), Some(b"v0".to_vec()));
        assert_eq!(db.get(b"key-0499").unwrap(), Some(b"v499".to_vec()));
        assert_eq!(db.get(b"absent").unwrap(), None);
        assert_eq!(db.get(b"key-9999").unwrap(), None);
    }
    // Reopen: sidecars are loaded; answers unchanged.
    let db = Lsm::open_with(dir.path(), no_compact()).unwrap();
    assert_eq!(db.get(b"key-0250").unwrap(), Some(b"v250".to_vec()));
    assert_eq!(db.get(b"missing").unwrap(), None);
    assert_eq!(db.scan(..).unwrap().count(), 500);
}

#[test]
fn missing_sidecar_degrades_gracefully() {
    let dir = tempfile::tempdir().unwrap();
    {
        let db = Lsm::open_with(dir.path(), no_compact()).unwrap();
        db.put(b"a", b"1").unwrap();
        db.put(b"b", b"2").unwrap();
        db.flush().unwrap();
    }
    // Delete the sidecar: the run is still authoritative.
    for bloom in files_with(dir.path(), ".sst.bloom") {
        fs::remove_file(bloom).unwrap();
    }
    let db = Lsm::open_with(dir.path(), no_compact()).unwrap();
    assert_eq!(db.get(b"a").unwrap(), Some(b"1".to_vec()));
    assert_eq!(db.get(b"b").unwrap(), Some(b"2".to_vec()));
    assert_eq!(db.get(b"absent").unwrap(), None);
}

#[test]
fn corrupt_sidecar_does_not_change_answers() {
    let dir = tempfile::tempdir().unwrap();
    {
        let db = Lsm::open_with(dir.path(), no_compact()).unwrap();
        db.put(b"x", b"42").unwrap();
        db.flush().unwrap();
    }
    for bloom in files_with(dir.path(), ".sst.bloom") {
        fs::write(bloom, b"garbage not a filter").unwrap();
    }
    let db = Lsm::open_with(dir.path(), no_compact()).unwrap();
    assert_eq!(db.get(b"x").unwrap(), Some(b"42".to_vec()));
    assert_eq!(db.get(b"y").unwrap(), None);
}

#[test]
fn orphan_sidecar_is_reclaimed_on_open() {
    let dir = tempfile::tempdir().unwrap();
    {
        let db = Lsm::open_with(dir.path(), no_compact()).unwrap();
        db.put(b"k", b"v").unwrap();
        db.flush().unwrap();
    }
    // A sidecar with no matching live run (e.g. from a crashed compaction).
    let orphan = dir.path().join("run-0000009999.sst.bloom");
    fs::write(&orphan, b"stale filter bytes").unwrap();

    let db = Lsm::open_with(dir.path(), no_compact()).unwrap();
    assert!(
        !orphan.exists(),
        "orphan sidecar should be reclaimed on open"
    );
    assert_eq!(db.get(b"k").unwrap(), Some(b"v".to_vec()));
}

#[test]
fn correct_under_background_compaction() {
    let dir = tempfile::tempdir().unwrap();
    let db = Lsm::open_with(dir.path(), LsmConfig::new().compaction_trigger(3)).unwrap();
    for i in 0..300u32 {
        db.put(format!("k{i:03}").into_bytes(), b"v").unwrap();
        if i % 20 == 19 {
            db.flush().unwrap();
        }
    }
    db.flush().unwrap();
    // Correctness holds regardless of how far background compaction has
    // progressed; sidecars track runs as compaction rewrites them.
    for i in 0..300u32 {
        assert_eq!(
            db.get(format!("k{i:03}").into_bytes()).unwrap(),
            Some(b"v".to_vec())
        );
    }
    assert_eq!(db.get(b"absent").unwrap(), None);
    assert_eq!(db.scan(..).unwrap().count(), 300);
}
