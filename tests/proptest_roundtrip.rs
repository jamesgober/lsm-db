//! Property tests for the v0.2 read/write contract.
//!
//! The engine is checked against a `BTreeMap` reference model: for any sequence
//! of puts and deletes, every `get` and a full `scan` must agree with the model,
//! and the data must survive a flush-and-reopen cycle. The memtable capacity is
//! varied so the same op sequence is exercised entirely in memory, with frequent
//! flushes, and flushing on every write.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;

use lsm_db::{Lsm, LsmConfig};
use proptest::prelude::*;

#[derive(Debug, Clone)]
enum Op {
    Put(Vec<u8>, Vec<u8>),
    Delete(Vec<u8>),
}

/// Keys are drawn from a tiny alphabet and short length so the same key recurs
/// often — that is what exercises overwrite, delete-then-put, and shadowing.
fn key() -> impl Strategy<Value = Vec<u8>> {
    proptest::collection::vec(0u8..6, 1..4)
}

fn value() -> impl Strategy<Value = Vec<u8>> {
    proptest::collection::vec(any::<u8>(), 0..8)
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        (key(), value()).prop_map(|(k, v)| Op::Put(k, v)),
        key().prop_map(Op::Delete),
    ]
}

fn apply(db: &Lsm, model: &mut BTreeMap<Vec<u8>, Vec<u8>>, ops: &[Op]) {
    for op in ops {
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
}

fn touched_keys(ops: &[Op]) -> Vec<Vec<u8>> {
    let mut keys: Vec<Vec<u8>> = ops
        .iter()
        .map(|op| match op {
            Op::Put(k, _) | Op::Delete(k) => k.clone(),
        })
        .collect();
    keys.sort();
    keys.dedup();
    keys
}

proptest! {
    /// `get` and a full `scan` agree with the model after any op sequence, for
    /// every write-buffer size.
    #[test]
    fn get_and_scan_match_model(
        ops in proptest::collection::vec(op(), 0..200),
        cap in prop_oneof![Just(0usize), Just(64usize), Just(4096usize)],
    ) {
        let dir = tempfile::tempdir().unwrap();
        let db = Lsm::open_with(dir.path(), LsmConfig::new().memtable_capacity(cap)).unwrap();
        let mut model = BTreeMap::new();
        apply(&db, &mut model, &ops);

        for k in touched_keys(&ops) {
            let got = db.get(&k).unwrap();
            prop_assert_eq!(got.as_deref(), model.get(&k).map(Vec::as_slice));
        }

        let scanned: Vec<_> = db.scan(..).unwrap().collect();
        let expected: Vec<_> = model.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        prop_assert_eq!(scanned, expected);
    }

    /// Every live key survives a flush, close, and reopen.
    #[test]
    fn reopen_after_flush_preserves_live_keys(ops in proptest::collection::vec(op(), 0..150)) {
        let dir = tempfile::tempdir().unwrap();
        let mut model = BTreeMap::new();
        {
            let db = Lsm::open(dir.path()).unwrap();
            apply(&db, &mut model, &ops);
            db.flush().unwrap();
        }

        let db = Lsm::open(dir.path()).unwrap();
        let scanned: Vec<_> = db.scan(..).unwrap().collect();
        let expected: Vec<_> = model.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        prop_assert_eq!(scanned, expected);
    }

    /// A bounded sub-range scan matches the model restricted to that range.
    #[test]
    fn scan_subrange_matches_model(
        ops in proptest::collection::vec(op(), 0..120),
        lo in key(),
        hi in key(),
    ) {
        let (lo, hi) = if lo <= hi { (lo, hi) } else { (hi, lo) };
        let dir = tempfile::tempdir().unwrap();
        let db = Lsm::open_with(dir.path(), LsmConfig::new().memtable_capacity(128)).unwrap();
        let mut model = BTreeMap::new();
        apply(&db, &mut model, &ops);

        let scanned: Vec<_> = db.scan(lo.clone()..hi.clone()).unwrap().collect();
        let expected: Vec<_> = model
            .range(lo..hi)
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        prop_assert_eq!(scanned, expected);
    }
}
