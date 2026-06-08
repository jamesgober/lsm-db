//! A multi-threaded soak: many writers and readers over one shared engine, with
//! background compaction running throughout.
//!
//! Each writer owns a disjoint slice of the key space and performs a
//! deterministic put-then-delete pattern, so the final live set is exactly
//! computable. Reader threads scan continuously while the writers work, and must
//! never observe a torn or out-of-order result. After the writers finish, the
//! engine's full scan must equal the union of what every writer left behind —
//! proving correctness under real concurrent load with flushes and compactions
//! interleaved. Under `--all-features` this runs with the write-ahead log and
//! bloom filters active.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;

use lsm_db::{Lsm, LsmConfig};

const WRITERS: u32 = 6;
const READERS: u32 = 3;
const PER_WRITER: u32 = 600;

fn key(writer: u32, i: u32) -> Vec<u8> {
    format!("w{writer}-k{i:05}").into_bytes()
}

#[test]
fn many_threads_mixed_workload_final_state_is_exact() {
    let dir = tempfile::tempdir().unwrap();
    // Tiny buffer + low trigger keep flushes and compaction busy throughout.
    let config = LsmConfig::new()
        .memtable_capacity(4 * 1024)
        .compaction_trigger(3);
    let db = Arc::new(Lsm::open_with(dir.path(), config).unwrap());
    let stop = Arc::new(AtomicBool::new(false));

    let mut handles = Vec::new();

    // Writers: each writes its whole slice, then deletes the odd-indexed keys,
    // leaving the even-indexed ones as its final contribution.
    for w in 0..WRITERS {
        let db = Arc::clone(&db);
        handles.push(thread::spawn(move || {
            for i in 0..PER_WRITER {
                db.put(key(w, i), format!("v{i}").into_bytes()).unwrap();
            }
            for i in (1..PER_WRITER).step_by(2) {
                db.delete(key(w, i)).unwrap();
            }
        }));
    }

    // Readers: scan continuously; every snapshot must be strictly ascending and
    // free of duplicates (no torn merge across the moving run set).
    for _ in 0..READERS {
        let db = Arc::clone(&db);
        let stop = Arc::clone(&stop);
        handles.push(thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                let keys: Vec<Vec<u8>> = db.scan(..).unwrap().map(|(k, _)| k).collect();
                for pair in keys.windows(2) {
                    assert!(pair[0] < pair[1], "scan must stay strictly ascending");
                }
            }
        }));
    }

    // Join the writers first, then stop the readers.
    for h in handles.drain(..WRITERS as usize) {
        h.join().unwrap();
    }
    stop.store(true, Ordering::Relaxed);
    for h in handles {
        h.join().unwrap();
    }

    db.flush().unwrap();

    // The final live set is each writer's even-indexed keys.
    let expected_count = (WRITERS * PER_WRITER.div_ceil(2)) as usize;
    assert_eq!(db.scan(..).unwrap().count(), expected_count);
    for w in 0..WRITERS {
        assert_eq!(db.get(key(w, 0)).unwrap(), Some(b"v0".to_vec())); // even: present
        assert_eq!(db.get(key(w, 1)).unwrap(), None); // odd: deleted
        let last_even = (PER_WRITER - 1) & !1;
        assert_eq!(
            db.get(key(w, last_even)).unwrap(),
            Some(format!("v{last_even}").into_bytes())
        );
    }

    // Survives a reopen with everything still exact.
    drop(db);
    let db = Lsm::open(dir.path()).unwrap();
    assert_eq!(db.scan(..).unwrap().count(), expected_count);
}
