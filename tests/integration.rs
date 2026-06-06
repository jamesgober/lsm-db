//! End-to-end tests over the public API.
//!
//! These drive [`Lsm`] the way a consumer would: through `open`, `put`, `get`,
//! `delete`, `scan`, `write`, and reopen — including data sets large enough to
//! span several automatic flushes.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use lsm_db::{Batch, Lsm, LsmConfig};

#[test]
fn many_writes_across_flushes_then_reopen() {
    let dir = tempfile::tempdir().unwrap();
    // A small buffer forces dozens of flushes over the run below.
    let config = LsmConfig::new().memtable_capacity(512);

    {
        let db = Lsm::open_with(dir.path(), config.clone()).unwrap();
        for i in 0..2_000u32 {
            let key = format!("key-{i:05}");
            let val = format!("value-{i}");
            db.put(key.as_bytes(), val.as_bytes()).unwrap();
        }
        // Overwrite a slice with new values, delete another slice.
        for i in 0..500u32 {
            let key = format!("key-{i:05}");
            db.put(key.as_bytes(), b"updated").unwrap();
        }
        for i in 1_500..2_000u32 {
            let key = format!("key-{i:05}");
            db.delete(key.as_bytes()).unwrap();
        }
        db.flush().unwrap();
    }

    let db = Lsm::open_with(dir.path(), config).unwrap();

    // Updated slice.
    assert_eq!(db.get(b"key-00000").unwrap(), Some(b"updated".to_vec()));
    assert_eq!(db.get(b"key-00499").unwrap(), Some(b"updated".to_vec()));
    // Untouched slice keeps its original value.
    assert_eq!(db.get(b"key-00500").unwrap(), Some(b"value-500".to_vec()));
    assert_eq!(db.get(b"key-01499").unwrap(), Some(b"value-1499".to_vec()));
    // Deleted slice is gone.
    assert_eq!(db.get(b"key-01500").unwrap(), None);
    assert_eq!(db.get(b"key-01999").unwrap(), None);

    // The live key count is 1_500 (2_000 written, 500 deleted).
    assert_eq!(db.scan(..).unwrap().count(), 1_500);
}

#[test]
fn scan_returns_keys_in_sorted_order() {
    let dir = tempfile::tempdir().unwrap();
    let db = Lsm::open(dir.path()).unwrap();
    for key in ["delta", "alpha", "charlie", "bravo"] {
        db.put(key.as_bytes(), b"x").unwrap();
    }
    let keys: Vec<_> = db.scan(..).unwrap().map(|(k, _)| k).collect();
    assert_eq!(
        keys,
        vec![
            b"alpha".to_vec(),
            b"bravo".to_vec(),
            b"charlie".to_vec(),
            b"delta".to_vec()
        ]
    );
}

#[test]
fn batch_is_visible_atomically_after_write() {
    let dir = tempfile::tempdir().unwrap();
    let db = Lsm::open(dir.path()).unwrap();
    db.put(b"existing", b"old").unwrap();

    let mut batch = Batch::new();
    for i in 0..100u32 {
        batch.put(format!("b{i:03}").into_bytes(), b"v");
    }
    batch.delete(b"existing");
    db.write(batch).unwrap();

    assert_eq!(db.get(b"b000").unwrap(), Some(b"v".to_vec()));
    assert_eq!(db.get(b"b099").unwrap(), Some(b"v".to_vec()));
    assert_eq!(db.get(b"existing").unwrap(), None);
}

#[test]
fn concurrent_readers_and_writer_share_one_engine() {
    use std::sync::Arc;
    use std::thread;

    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(Lsm::open(dir.path()).unwrap());

    // Seed some data.
    for i in 0..100u32 {
        db.put(format!("k{i:03}").into_bytes(), b"v").unwrap();
    }

    let mut handles = Vec::new();
    // One writer thread.
    {
        let db = Arc::clone(&db);
        handles.push(thread::spawn(move || {
            for i in 100..300u32 {
                db.put(format!("k{i:03}").into_bytes(), b"v").unwrap();
            }
        }));
    }
    // Several reader threads, scanning concurrently.
    for _ in 0..4 {
        let db = Arc::clone(&db);
        handles.push(thread::spawn(move || {
            for _ in 0..50 {
                // A scan must never observe a torn state; counts stay in range.
                let n = db.scan(..).unwrap().count();
                assert!((100..=300).contains(&n));
            }
        }));
    }

    for h in handles {
        h.join().unwrap();
    }
    assert_eq!(db.scan(..).unwrap().count(), 300);
}
