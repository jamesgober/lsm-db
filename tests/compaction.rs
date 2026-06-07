//! Compaction correctness: under sustained writes and background compaction, no
//! live key is ever lost or duplicated.
//!
//! These drive the public API with a small memtable and a low compaction trigger
//! so the background compactor runs repeatedly while the test writes. Because the
//! engine stays correct at every moment regardless of when compaction fires, the
//! invariants are asserted after the workload: a full scan equals the reference
//! model exactly (no loss, no duplication), and every key reads back correctly.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::sync::Arc;
use std::thread;

use lsm_db::{Lsm, LsmConfig};
use proptest::prelude::*;

#[derive(Debug, Clone)]
enum Op {
    Put(Vec<u8>, Vec<u8>),
    Delete(Vec<u8>),
}

fn key() -> impl Strategy<Value = Vec<u8>> {
    proptest::collection::vec(0u8..8, 1..4)
}

fn value() -> impl Strategy<Value = Vec<u8>> {
    proptest::collection::vec(any::<u8>(), 0..12)
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        (key(), value()).prop_map(|(k, v)| Op::Put(k, v)),
        key().prop_map(Op::Delete),
    ]
}

proptest! {
    /// A workload run against a small, frequently-compacting engine matches the
    /// model exactly once it settles.
    #[test]
    fn compaction_preserves_model(ops in proptest::collection::vec(op(), 0..400)) {
        let dir = tempfile::tempdir().unwrap();
        let config = LsmConfig::new().memtable_capacity(256).compaction_trigger(3);
        let db = Lsm::open_with(dir.path(), config).unwrap();

        let mut model: BTreeMap<Vec<u8>, Vec<u8>> = BTreeMap::new();
        for op in &ops {
            match op {
                Op::Put(k, v) => {
                    db.put(k, v).unwrap();
                    let _ = model.insert(k.clone(), v.clone());
                }
                Op::Delete(k) => {
                    db.delete(k).unwrap();
                    let _ = model.remove(k);
                }
            }
        }
        db.flush().unwrap();

        let scanned: Vec<_> = db.scan(..).unwrap().collect();
        let expected: Vec<_> = model.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        prop_assert_eq!(scanned, expected);
    }
}

#[test]
fn concurrent_writers_with_background_compaction() {
    let dir = tempfile::tempdir().unwrap();
    // Tiny buffer + low trigger keeps the compactor busy the whole run.
    let config = LsmConfig::new()
        .memtable_capacity(512)
        .compaction_trigger(3);
    let db = Arc::new(Lsm::open_with(dir.path(), config).unwrap());

    const WRITERS: u32 = 4;
    const PER_WRITER: u32 = 1_000;

    let mut handles = Vec::new();
    for w in 0..WRITERS {
        let db = Arc::clone(&db);
        handles.push(thread::spawn(move || {
            for i in 0..PER_WRITER {
                // Disjoint key spaces per writer, so the final state is exact.
                let key = format!("w{w}-{i:05}");
                db.put(key.as_bytes(), b"v").unwrap();
            }
        }));
    }
    // A reader thread scanning throughout must never see a torn or duplicated key.
    {
        let db = Arc::clone(&db);
        handles.push(thread::spawn(move || {
            for _ in 0..40 {
                let keys: Vec<_> = db.scan(..).unwrap().map(|(k, _)| k).collect();
                for pair in keys.windows(2) {
                    assert!(pair[0] < pair[1], "scan keys must be strictly increasing");
                }
            }
        }));
    }
    for h in handles {
        h.join().unwrap();
    }

    db.flush().unwrap();
    let total = (WRITERS * PER_WRITER) as usize;
    assert_eq!(db.scan(..).unwrap().count(), total);

    // Spot-check every key is present exactly once.
    let keys: Vec<_> = db.scan(..).unwrap().map(|(k, _)| k).collect();
    assert_eq!(keys.len(), total);
    for pair in keys.windows(2) {
        assert!(pair[0] < pair[1]);
    }
    for w in 0..WRITERS {
        assert_eq!(
            db.get(format!("w{w}-00000").into_bytes()).unwrap(),
            Some(b"v".to_vec())
        );
        assert_eq!(
            db.get(format!("w{w}-00999").into_bytes()).unwrap(),
            Some(b"v".to_vec())
        );
    }
}
