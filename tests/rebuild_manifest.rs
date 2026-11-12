//! `Lsm::rebuild_manifest`: the explicit recovery for a store whose manifest
//! is lost or damaged.
//!
//! Before 1.3 a lost manifest made the next open delete every run; 1.3 and
//! later refuse the open and keep the runs, and this call writes a manifest
//! for them again, newest first by sequence number.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use lsm_db::{Error, Lsm, LsmConfig};

fn run_files(dir: &Path) -> Vec<PathBuf> {
    let mut runs: Vec<PathBuf> = fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("run-") && n.ends_with(".sst"))
        })
        .collect();
    runs.sort();
    runs
}

/// A store whose runs disagree, so only the right order reads back the
/// right values: a compacted run, then two flushes that overwrite and
/// delete keys in it.
fn layered_store(dir: &Path) {
    let db = Lsm::open_with(dir, LsmConfig::new().compaction_trigger(usize::MAX)).unwrap();
    db.put(b"a", b"1").unwrap();
    db.put(b"b", b"1").unwrap();
    db.put(b"c", b"1").unwrap();
    db.flush().unwrap();
    db.put(b"d", b"1").unwrap();
    db.flush().unwrap();
    db.close().unwrap();
    // A third run reaches the trigger: the three are compacted into one.
    let db = Lsm::open_with(dir, LsmConfig::new().compaction_trigger(2)).unwrap();
    db.put(b"a", b"2").unwrap();
    db.delete(b"b").unwrap();
    db.flush().unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while run_files(dir).len() != 1 {
        assert!(std::time::Instant::now() < deadline, "no compaction");
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    db.close().unwrap();
    let db = Lsm::open_with(dir, LsmConfig::new().compaction_trigger(usize::MAX)).unwrap();
    db.put(b"a", b"3").unwrap();
    db.put(b"b", b"3").unwrap();
    db.delete(b"c").unwrap();
    db.flush().unwrap();
    db.close().unwrap();
}

fn contents(dir: &Path) -> Vec<(Vec<u8>, Vec<u8>)> {
    Lsm::open(dir).unwrap().scan(..).unwrap().collect()
}

#[test]
fn a_lost_manifest_is_rebuilt_in_recency_order() {
    let dir = tempfile::tempdir().unwrap();
    layered_store(dir.path());
    let expected = contents(dir.path());
    assert_eq!(
        expected,
        vec![
            (b"a".to_vec(), b"3".to_vec()),
            (b"b".to_vec(), b"3".to_vec()),
            (b"d".to_vec(), b"1".to_vec()),
        ]
    );
    let runs = run_files(dir.path()).len();
    assert!(runs >= 2, "{runs}");

    fs::remove_file(dir.path().join("MANIFEST")).unwrap();
    assert!(matches!(
        Lsm::open(dir.path()),
        Err(Error::Corruption { .. })
    ));
    assert_eq!(Lsm::rebuild_manifest(dir.path()).unwrap(), runs);
    assert!(!dir.path().join("MANIFEST.old").exists());
    assert_eq!(contents(dir.path()), expected);
}

#[test]
fn a_damaged_manifest_is_kept_aside_and_rebuilt() {
    let dir = tempfile::tempdir().unwrap();
    layered_store(dir.path());
    let expected = contents(dir.path());
    fs::write(dir.path().join("MANIFEST"), b"not a manifest").unwrap();
    assert!(Lsm::open(dir.path()).is_err());
    let _ = Lsm::rebuild_manifest(dir.path()).unwrap();
    assert_eq!(
        fs::read(dir.path().join("MANIFEST.old")).unwrap(),
        b"not a manifest"
    );
    assert_eq!(contents(dir.path()), expected);
}

#[test]
fn an_intact_manifest_is_not_rebuilt() {
    let dir = tempfile::tempdir().unwrap();
    layered_store(dir.path());
    let manifest = fs::read(dir.path().join("MANIFEST")).unwrap();
    assert!(matches!(
        Lsm::rebuild_manifest(dir.path()),
        Err(Error::InvalidInput { .. })
    ));
    assert_eq!(fs::read(dir.path().join("MANIFEST")).unwrap(), manifest);
}

#[test]
fn a_rebuild_is_refused_while_the_store_is_open() {
    let dir = tempfile::tempdir().unwrap();
    let db = Lsm::open(dir.path()).unwrap();
    let result = Lsm::rebuild_manifest(dir.path());
    assert!(
        matches!(&result, Err(Error::Io { source, .. }) if source.kind() == ErrorKind::WouldBlock),
        "{result:?}"
    );
    drop(db);
}

#[test]
fn a_damaged_run_stops_the_rebuild_before_anything_is_written() {
    let dir = tempfile::tempdir().unwrap();
    layered_store(dir.path());
    fs::remove_file(dir.path().join("MANIFEST")).unwrap();
    let run = run_files(dir.path()).pop().unwrap();
    let mut bytes = fs::read(&run).unwrap();
    let last = bytes.len() - 1;
    bytes[last] ^= 0xFF; // the footer magic
    fs::write(&run, &bytes).unwrap();
    assert!(matches!(
        Lsm::rebuild_manifest(dir.path()),
        Err(Error::Corruption { .. })
    ));
    assert!(!dir.path().join("MANIFEST").exists());
}
