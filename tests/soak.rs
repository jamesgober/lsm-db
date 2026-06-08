//! A sustained, consumer-shaped workload across restarts.
//!
//! This is the kind of use a real consumer (a transactional store, a record
//! engine) puts the index through: tens of thousands of interleaved
//! puts, overwrites, and deletes, with the buffer small enough and the
//! compaction trigger low enough that flushes and background compactions run
//! throughout — punctuated by close-and-reopen cycles, the way a process restart
//! looks. After every phase the engine is checked, key for key and over a full
//! scan, against a `BTreeMap` reference model. Under `--all-features` the same
//! workload also exercises the write-ahead log and bloom filters together.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;

use lsm_db::{Lsm, LsmConfig};

/// Deterministic SplitMix64, so the workload is identical on every run and
/// platform.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

/// Keys drawn from a bounded space, so overwrites and deletes recur and the tree
/// of versions stays interesting rather than write-once.
const KEY_SPACE: u64 = 4_000;
const OPS_PER_PHASE: u32 = 8_000;
const PHASES: u32 = 4;

fn config() -> LsmConfig {
    // Small buffer + low trigger: many flushes, frequent background compaction.
    LsmConfig::new()
        .memtable_capacity(8 * 1024)
        .compaction_trigger(3)
}

fn key(k: u64) -> Vec<u8> {
    format!("key-{k:06}").into_bytes()
}

fn apply_phase(db: &Lsm, model: &mut BTreeMap<Vec<u8>, Vec<u8>>, rng: &mut Rng, phase: u32) {
    for op in 0..OPS_PER_PHASE {
        let k = key(rng.below(KEY_SPACE));
        // ~70% writes, ~30% deletes — a write-leaning mixed workload.
        if rng.below(10) < 7 {
            let v = format!("p{phase}-op{op}").into_bytes();
            db.put(&k, &v).unwrap();
            let _ = model.insert(k, v);
        } else {
            db.delete(&k).unwrap();
            let _ = model.remove(&k);
        }
    }
}

fn verify(db: &Lsm, model: &BTreeMap<Vec<u8>, Vec<u8>>) {
    // Full scan equals the model exactly: no lost, duplicated, or resurrected key.
    let scanned: Vec<(Vec<u8>, Vec<u8>)> = db.scan(..).unwrap().collect();
    let expected: Vec<(Vec<u8>, Vec<u8>)> =
        model.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
    assert_eq!(scanned.len(), expected.len(), "live key count diverged");
    assert_eq!(scanned, expected, "scan diverged from the model");

    // Point reads agree across the whole key space, present and absent alike.
    for k in 0..KEY_SPACE {
        let key = key(k);
        assert_eq!(
            db.get(&key).unwrap().as_deref(),
            model.get(&key).map(Vec::as_slice)
        );
    }
}

#[test]
fn sustained_workload_survives_restarts() {
    let dir = tempfile::tempdir().unwrap();
    let mut model: BTreeMap<Vec<u8>, Vec<u8>> = BTreeMap::new();
    let mut rng = Rng(0xC0FF_EE12_3456_789A);

    for phase in 0..PHASES {
        // Each phase opens fresh (a process restart), works, verifies, and closes.
        let db = Lsm::open_with(dir.path(), config()).unwrap();
        apply_phase(&db, &mut model, &mut rng, phase);
        db.flush().unwrap();
        verify(&db, &model);
        // Drop joins the background compactor; the next phase reopens.
    }

    // One final reopen with the default config (default buffer, default cache)
    // confirms everything persisted is still exactly the model.
    let db = Lsm::open(dir.path()).unwrap();
    verify(&db, &model);
}

#[test]
fn ranged_scans_track_the_model_under_churn() {
    let dir = tempfile::tempdir().unwrap();
    let db = Lsm::open_with(dir.path(), config()).unwrap();
    let mut model: BTreeMap<Vec<u8>, Vec<u8>> = BTreeMap::new();
    let mut rng = Rng(0x1234_5678_9ABC_DEF0);

    apply_phase(&db, &mut model, &mut rng, 0);
    db.flush().unwrap();

    // Several bounded ranges must each match the model restricted to that range.
    for _ in 0..32 {
        let a = key(rng.below(KEY_SPACE));
        let b = key(rng.below(KEY_SPACE));
        let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
        let got: Vec<(Vec<u8>, Vec<u8>)> = db.scan(lo.clone()..hi.clone()).unwrap().collect();
        let expected: Vec<(Vec<u8>, Vec<u8>)> = model
            .range(lo..hi)
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        assert_eq!(got, expected);
    }
}
