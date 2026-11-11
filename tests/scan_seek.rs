//! Scans that seek to their start key return what a full scan filtered to
//! the range returns: checked against a `BTreeMap` model over several runs
//! and the memtable, for every kind of bound, including empty and inverted
//! ranges.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::ops::Bound;

use lsm_db::{Lsm, LsmConfig};

/// A small deterministic generator (64-bit LCG), so a failure repeats.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        self.0 >> 33
    }

    fn key(&mut self) -> Vec<u8> {
        format!("k{:05}", self.next() % 4000).into_bytes()
    }

    fn bound(&mut self) -> Bound<Vec<u8>> {
        match self.next() % 3 {
            0 => Bound::Included(self.key()),
            1 => Bound::Excluded(self.key()),
            _ => Bound::Unbounded,
        }
    }
}

fn model_scan(
    model: &BTreeMap<Vec<u8>, Vec<u8>>,
    start: &Bound<Vec<u8>>,
    end: &Bound<Vec<u8>>,
) -> Vec<(Vec<u8>, Vec<u8>)> {
    model
        .iter()
        .filter(|(k, _)| {
            let after_start = match start {
                Bound::Included(s) => *k >= s,
                Bound::Excluded(s) => *k > s,
                Bound::Unbounded => true,
            };
            let before_end = match end {
                Bound::Included(e) => *k <= e,
                Bound::Excluded(e) => *k < e,
                Bound::Unbounded => true,
            };
            after_start && before_end
        })
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect()
}

#[test]
fn test_seeking_scans_match_the_model() {
    let dir = tempfile::tempdir().unwrap();
    // Small blocks' worth of data per run, no compaction: many runs, each
    // with many blocks, and overwrites and deletes across them.
    let db = Lsm::open_with(dir.path(), LsmConfig::new().compaction_trigger(usize::MAX)).unwrap();
    let mut model = BTreeMap::new();
    let mut rng = Rng(0x5EED);
    for round in 0..6 {
        for _ in 0..1500 {
            let key = rng.key();
            if rng.next() % 5 == 0 {
                db.delete(&key).unwrap();
                let _ = model.remove(&key);
            } else {
                let value = format!("v{round}-{}", rng.next()).into_bytes();
                db.put(&key, &value).unwrap();
                let _ = model.insert(key, value);
            }
        }
        // The last round stays in the memtable.
        if round < 5 {
            db.flush().unwrap();
        }
    }
    for case in 0..2000 {
        let (start, end) = (rng.bound(), rng.bound());
        let got: Vec<_> = db.scan((start.clone(), end.clone())).unwrap().collect();
        assert_eq!(
            got,
            model_scan(&model, &start, &end),
            "case {case}: {start:?}..{end:?}"
        );
    }
    // The equal-and-excluded and inverted ranges are empty, never a panic.
    let k = b"k02000".to_vec();
    assert_eq!(
        db.scan((Bound::Excluded(k.clone()), Bound::Excluded(k.clone())))
            .unwrap()
            .count(),
        0
    );
    assert_eq!(
        db.scan((Bound::Included(k.clone()), Bound::Excluded(k.clone())))
            .unwrap()
            .count(),
        0
    );
    assert_eq!(
        db.scan(b"k03000".to_vec()..b"k01000".to_vec())
            .unwrap()
            .count(),
        0
    );
}

#[test]
fn test_scan_prefix() {
    let dir = tempfile::tempdir().unwrap();
    let db = Lsm::open(dir.path()).unwrap();
    for key in [
        &b"ab"[..],
        b"ab\x00",
        b"ab\xff",
        b"ac",
        b"a\xff",
        b"b",
        b"\xff",
        b"\xff\xff",
        b"\xff\xff\x01",
    ] {
        db.put(key, b"v").unwrap();
    }
    db.flush().unwrap();
    db.put(b"ab\x7f", b"v").unwrap();
    let keys = |prefix: &[u8]| -> Vec<Vec<u8>> {
        db.scan_prefix(prefix).unwrap().map(|(k, _)| k).collect()
    };
    assert_eq!(
        keys(b"ab"),
        vec![
            b"ab".to_vec(),
            b"ab\x00".to_vec(),
            b"ab\x7f".to_vec(),
            b"ab\xff".to_vec()
        ]
    );
    assert_eq!(keys(b"a\xff"), vec![b"a\xff".to_vec()]);
    // A prefix of only 0xFF bytes runs to the end of the key space.
    assert_eq!(
        keys(b"\xff\xff"),
        vec![b"\xff\xff".to_vec(), b"\xff\xff\x01".to_vec()]
    );
    assert_eq!(keys(b"\xff").len(), 3);
    // The empty prefix is every key.
    assert_eq!(keys(b"").len(), 10);
    assert!(keys(b"zz").is_empty());
}

#[test]
fn test_close_keeps_writes_and_reports() {
    let dir = tempfile::tempdir().unwrap();
    let db = Lsm::open(dir.path()).unwrap();
    db.put(b"k", b"v").unwrap();
    db.close().unwrap();
    let db = Lsm::open(dir.path()).unwrap();
    assert_eq!(db.get(b"k").unwrap(), Some(b"v".to_vec()));
}
