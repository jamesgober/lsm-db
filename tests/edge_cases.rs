//! Edge cases: large values, many runs, unusual keys, and graceful failure on
//! an I/O error. None of these should panic; failures surface as `Err`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use lsm_db::{Lsm, LsmConfig};

#[test]
fn multi_megabyte_value_round_trips() {
    let dir = tempfile::tempdir().unwrap();
    let big = vec![0x5Au8; 4 * 1024 * 1024]; // 4 MiB, far larger than a block
    {
        let db = Lsm::open(dir.path()).unwrap();
        db.put(b"big", &big).unwrap();
        db.put(b"small", b"x").unwrap();
        db.flush().unwrap();
        assert_eq!(db.get(b"big").unwrap(), Some(big.clone()));
    }
    // Survives reopen (read back from the run, large block and all).
    let db = Lsm::open(dir.path()).unwrap();
    assert_eq!(db.get(b"big").unwrap(), Some(big));
    assert_eq!(db.get(b"small").unwrap(), Some(b"x".to_vec()));
}

#[test]
fn many_runs_read_and_scan_correctly() {
    let dir = tempfile::tempdir().unwrap();
    // No auto-compaction: 50 separate runs the read path must merge across.
    let db = Lsm::open_with(dir.path(), LsmConfig::new().compaction_trigger(usize::MAX)).unwrap();
    for run in 0..50u32 {
        db.put(
            format!("k{run:03}").into_bytes(),
            format!("v{run}").into_bytes(),
        )
        .unwrap();
        db.flush().unwrap();
    }
    // 50 separate runs (no compaction); the read path must merge across them.
    for run in 0..50u32 {
        assert_eq!(
            db.get(format!("k{run:03}").into_bytes()).unwrap(),
            Some(format!("v{run}").into_bytes())
        );
    }
    assert_eq!(db.scan(..).unwrap().count(), 50);
}

#[test]
fn empty_key_and_value_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    let db = Lsm::open(dir.path()).unwrap();
    db.put(b"", b"empty-key").unwrap();
    db.put(b"empty-value", b"").unwrap();
    db.flush().unwrap();
    assert_eq!(db.get(b"").unwrap(), Some(b"empty-key".to_vec()));
    assert_eq!(db.get(b"empty-value").unwrap(), Some(Vec::new()));
}

#[test]
fn very_long_key_round_trips() {
    let dir = tempfile::tempdir().unwrap();
    let db = Lsm::open(dir.path()).unwrap();
    let long_key = vec![0xAB; 64 * 1024]; // 64 KiB key
    db.put(&long_key, b"v").unwrap();
    db.flush().unwrap();
    assert_eq!(db.get(&long_key).unwrap(), Some(b"v".to_vec()));
}

#[test]
fn io_failure_on_flush_is_an_error_not_a_panic() {
    let dir = tempfile::tempdir().unwrap();
    let db = Lsm::open(dir.path()).unwrap();
    db.put(b"k", b"v").unwrap(); // buffered, not yet on disk

    // Remove the database directory out from under the engine. The next flush
    // cannot create its run file and must surface that as an error, not a panic.
    std::fs::remove_dir_all(dir.path()).unwrap();
    assert!(
        db.flush().is_err(),
        "flush into a missing directory must error"
    );
}

/// Keys and values longer than the run format allows (1 GiB) are rejected up
/// front. Before 1.1 they were accepted and the flushed run then failed to
/// read back. The oversized buffers are zero-filled, so the OS backs them
/// lazily and the test stays cheap.
#[test]
fn oversized_key_or_value_is_rejected_before_anything_is_written() {
    use lsm_db::{Batch, Error, format};

    let dir = tempfile::tempdir().unwrap();
    let db = Lsm::open(dir.path()).unwrap();
    let too_long = vec![0u8; format::MAX_KEY_LEN + 1];

    assert!(matches!(
        db.put(&too_long, b"v"),
        Err(Error::InvalidInput { .. })
    ));
    assert!(matches!(
        db.put(b"k", &too_long),
        Err(Error::InvalidInput { .. })
    ));
    assert!(matches!(
        db.delete(&too_long),
        Err(Error::InvalidInput { .. })
    ));

    // One bad entry rejects the whole batch.
    let mut batch = Batch::new();
    batch.put(b"good", b"1");
    batch.put(b"bad", &too_long);
    assert!(matches!(db.write(batch), Err(Error::InvalidInput { .. })));
    drop(too_long);

    assert_eq!(db.get(b"good").unwrap(), None);
    assert_eq!(db.get(b"k").unwrap(), None);
    db.put(b"k", b"v").unwrap();
    db.flush().unwrap();
    assert_eq!(db.get(b"k").unwrap(), Some(b"v".to_vec()));
}
