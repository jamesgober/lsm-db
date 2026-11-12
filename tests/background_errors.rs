//! A background compaction that fails is reported and cleans up after
//! itself.
//!
//! Its error used to be stored where no call could read it, and the
//! partial output stayed on disk (as large as the merge had got) until the
//! next open. Now `Lsm::take_compaction_error` returns it, the files the
//! compaction made are removed, every input run stays live, and reads and
//! writes go on.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::path::Path;
use std::time::{Duration, Instant};

use lsm_db::{Lsm, LsmConfig};

fn leftovers(dir: &Path) -> Vec<String> {
    fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n.ends_with(".tmp"))
        .collect()
}

#[test]
fn a_failed_background_compaction_is_reported_and_leaves_no_files() {
    let dir = tempfile::tempdir().unwrap();
    // Runs 0, 1, and 2 are flushes; the third reaches the trigger, and the
    // compaction's output is run 3. A directory with that name makes the
    // compaction's final rename fail after the whole merge is written.
    let db = Lsm::open_with(dir.path(), LsmConfig::new().compaction_trigger(3)).unwrap();
    for i in 0..2u32 {
        for j in 0..2_000u32 {
            db.put(format!("k{i}-{j:05}"), [b'x'; 64]).unwrap();
        }
        db.flush().unwrap();
    }
    let blocker = dir.path().join("run-0000000003.sst");
    fs::create_dir(&blocker).unwrap();
    for j in 0..2_000u32 {
        db.put(format!("k2-{j:05}"), [b'x'; 64]).unwrap();
    }
    db.flush().unwrap();

    let deadline = Instant::now() + Duration::from_secs(30);
    let err = loop {
        if let Some(err) = db.take_compaction_error() {
            break err;
        }
        assert!(
            Instant::now() < deadline,
            "no compaction error was reported"
        );
        std::thread::sleep(Duration::from_millis(20));
    };
    assert!(matches!(err, lsm_db::Error::Io { .. }), "{err:?}");
    assert!(db.take_compaction_error().is_none(), "taken once");
    assert!(
        leftovers(dir.path()).is_empty(),
        "{:?}",
        leftovers(dir.path())
    );

    // Every write is still there, and the store takes more.
    assert_eq!(db.scan(..).unwrap().count(), 6_000);
    db.put(b"after", b"1").unwrap();

    // With the obstacle gone, the next compaction succeeds and the data
    // survives a reopen.
    fs::remove_dir(&blocker).unwrap();
    db.flush().unwrap();
    drop(db);
    let db = Lsm::open(dir.path()).unwrap();
    assert_eq!(db.scan(..).unwrap().count(), 6_001);
}
