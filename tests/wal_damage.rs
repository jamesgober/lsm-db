//! The write-ahead log (`durability`) under damage and at its limits.
//!
//! - Damage in the middle of the log, with intact records after it, refuses
//!   the open and leaves the log as it is. It used to cut the log at the
//!   damage and open, dropping every acknowledged write after it.
//! - A write larger than wal-db's default 64 MiB record limit (but within
//!   the format's 1 GiB per key and value) is logged, as the docs promise.

#![cfg(feature = "durability")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::path::Path;

use lsm_db::{Error, Lsm, LsmConfig};

/// Copy every file in `from` to `to`, except the lock file.
fn copy_files(from: &Path, to: &Path) {
    for entry in fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        if entry.file_name() != "LOCK" {
            let _ = fs::copy(entry.path(), to.join(entry.file_name())).unwrap();
        }
    }
}

/// End a handle as a crash would (see `tests/durability.rs`).
fn crash(db: Lsm, dir: &Path) {
    let saved = tempfile::tempdir().unwrap();
    copy_files(dir, saved.path());
    drop(db);
    for entry in fs::read_dir(dir).unwrap() {
        fs::remove_file(entry.unwrap().path()).unwrap();
    }
    copy_files(saved.path(), dir);
}

fn durable(dir: &Path) -> Lsm {
    Lsm::open_with(dir, LsmConfig::new().compaction_trigger(usize::MAX)).unwrap()
}

#[test]
fn damage_in_the_middle_of_the_log_refuses_the_open() {
    let dir = tempfile::tempdir().unwrap();
    let db = durable(dir.path());
    for k in [b"k0", b"k1", b"k2"] {
        db.put(k, [b'v'; 100]).unwrap();
    }
    crash(db, dir.path());

    let wal = dir.path().join("wal.log");
    let mut bytes = fs::read(&wal).unwrap();
    let middle = bytes.len() / 2;
    bytes[middle] ^= 0xFF;
    fs::write(&wal, &bytes).unwrap();

    match Lsm::open(dir.path()) {
        Err(Error::Corruption { .. }) => {}
        Err(other) => panic!("expected corruption, got {other:?}"),
        Ok(db) => {
            let present: Vec<bool> = [b"k0", b"k1", b"k2"]
                .iter()
                .map(|k| db.get(k).unwrap().is_some())
                .collect();
            panic!("the open cut the log and succeeded; k0, k1, k2 present: {present:?}");
        }
    }
    assert_eq!(fs::read(&wal).unwrap(), bytes, "the log is left as it was");
}

#[test]
fn a_torn_final_record_is_still_cut() {
    let dir = tempfile::tempdir().unwrap();
    let db = durable(dir.path());
    db.put(b"k0", b"v0").unwrap();
    db.put(b"k1", b"v1").unwrap();
    crash(db, dir.path());

    let wal = dir.path().join("wal.log");
    let bytes = fs::read(&wal).unwrap();
    fs::write(&wal, &bytes[..bytes.len() - 2]).unwrap();
    let db = durable(dir.path());
    assert_eq!(db.get(b"k0").unwrap(), Some(b"v0".to_vec()));
    assert_eq!(
        db.get(b"k1").unwrap(),
        None,
        "the torn write was never acknowledged"
    );
}

#[test]
fn a_write_over_64_mib_is_logged_and_recovered() {
    let dir = tempfile::tempdir().unwrap();
    let value = vec![0x5Au8; 65 << 20];
    let db = durable(dir.path());
    db.put(b"big", &value).unwrap();
    crash(db, dir.path());
    let db = durable(dir.path());
    assert_eq!(db.get(b"big").unwrap(), Some(value));
}
