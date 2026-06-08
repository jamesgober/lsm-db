//! Adversarial / hostile-input hardening.
//!
//! Library code must never panic on a corrupted or truncated on-disk file, and
//! must never be tricked into an unbounded allocation by a hostile length
//! prefix. These property tests build a real database, then corrupt its files in
//! arbitrary ways and reopen it, asserting only that the engine returns a
//! `Result` — `Ok` or a corruption `Err`, never a panic, overflow, or hang.
//!
//! A panic anywhere in the reopen-and-query sequence fails the test, because
//! `proptest` runs each case on a thread whose panic is reported as a failure.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::path::{Path, PathBuf};

use lsm_db::{Lsm, LsmConfig};
use proptest::prelude::*;

/// One run file with no auto-compaction, so the corpus is predictable.
fn build_db(dir: &Path, entries: &[(Vec<u8>, Vec<u8>)]) {
    let db = Lsm::open_with(dir, LsmConfig::new().compaction_trigger(usize::MAX)).unwrap();
    for (k, v) in entries {
        db.put(k, v).unwrap();
    }
    db.flush().unwrap();
}

fn files_ending(dir: &Path, suffix: &str) -> Vec<PathBuf> {
    fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.ends_with(suffix))
        })
        .collect()
}

/// Reopen and exercise every read path; the only requirement is no panic.
fn reopen_and_query(dir: &Path, keys: &[Vec<u8>]) {
    if let Ok(db) = Lsm::open(dir) {
        for k in keys {
            let _ = db.get(k);
        }
        if let Ok(scan) = db.scan(..) {
            let _ = scan.count();
        }
    }
    // An `Err` from open (corruption rejected up front) is equally fine.
}

fn entries() -> impl Strategy<Value = Vec<(Vec<u8>, Vec<u8>)>> {
    proptest::collection::vec(
        (
            proptest::collection::vec(any::<u8>(), 1..8),
            proptest::collection::vec(any::<u8>(), 0..12),
        ),
        1..40,
    )
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(120))]

    /// Flipping arbitrary bytes of the run file never makes a reopen panic.
    #[test]
    fn corrupt_run_bytes_never_panics(
        data in entries(),
        flips in proptest::collection::vec((any::<u8>(), any::<u8>()), 0..24),
    ) {
        let dir = tempfile::tempdir().unwrap();
        build_db(dir.path(), &data);
        let run = files_ending(dir.path(), ".sst").pop().unwrap();

        let mut bytes = fs::read(&run).unwrap();
        if !bytes.is_empty() {
            for (pos, val) in &flips {
                let i = (*pos as usize) % bytes.len();
                bytes[i] ^= val.max(&1); // ensure a real change
            }
        }
        fs::write(&run, &bytes).unwrap();

        let keys: Vec<Vec<u8>> = data.iter().map(|(k, _)| k.clone()).collect();
        reopen_and_query(dir.path(), &keys);
    }

    /// Truncating the run file at an arbitrary point never makes a reopen panic.
    #[test]
    fn truncated_run_never_panics(data in entries(), cut in 0usize..4096) {
        let dir = tempfile::tempdir().unwrap();
        build_db(dir.path(), &data);
        let run = files_ending(dir.path(), ".sst").pop().unwrap();

        let bytes = fs::read(&run).unwrap();
        let keep = cut.min(bytes.len());
        fs::write(&run, &bytes[..keep]).unwrap();

        let keys: Vec<Vec<u8>> = data.iter().map(|(k, _)| k.clone()).collect();
        reopen_and_query(dir.path(), &keys);
    }

    /// Arbitrary garbage as the entire run file never makes a reopen panic — in
    /// particular, a hostile length prefix must not drive an unbounded
    /// allocation (the read path caps every length).
    #[test]
    fn arbitrary_run_bytes_never_panics(garbage in proptest::collection::vec(any::<u8>(), 0..2048)) {
        let dir = tempfile::tempdir().unwrap();
        // Seed a valid db so a manifest exists naming a run, then overwrite the
        // run with garbage.
        build_db(dir.path(), &[(b"k".to_vec(), b"v".to_vec())]);
        let run = files_ending(dir.path(), ".sst").pop().unwrap();
        fs::write(&run, &garbage).unwrap();
        reopen_and_query(dir.path(), &[b"k".to_vec()]);
    }

    /// Corrupting the manifest never makes a reopen panic.
    #[test]
    fn corrupt_manifest_never_panics(garbage in proptest::collection::vec(any::<u8>(), 0..256)) {
        let dir = tempfile::tempdir().unwrap();
        build_db(dir.path(), &[(b"k".to_vec(), b"v".to_vec())]);
        fs::write(dir.path().join("MANIFEST"), &garbage).unwrap();
        reopen_and_query(dir.path(), &[b"k".to_vec()]);
    }
}

#[cfg(feature = "durability")]
proptest! {
    #![proptest_config(ProptestConfig::with_cases(80))]

    /// Corrupting the write-ahead log never makes a reopen panic.
    #[test]
    fn corrupt_wal_never_panics(
        data in entries(),
        flips in proptest::collection::vec((any::<u8>(), any::<u8>()), 0..16),
    ) {
        let dir = tempfile::tempdir().unwrap();
        {
            // Durable writes, never flushed, so they live only in the log.
            let db = Lsm::open(dir.path()).unwrap();
            for (k, v) in &data {
                db.put(k, v).unwrap();
            }
        }
        let wal = dir.path().join("wal.log");
        if let Ok(mut bytes) = fs::read(&wal) {
            if !bytes.is_empty() {
                for (pos, val) in &flips {
                    let i = (*pos as usize) % bytes.len();
                    bytes[i] ^= val.max(&1);
                }
                fs::write(&wal, &bytes).unwrap();
            }
        }
        let keys: Vec<Vec<u8>> = data.iter().map(|(k, _)| k.clone()).collect();
        reopen_and_query(dir.path(), &keys);
    }
}

#[cfg(feature = "bloom")]
proptest! {
    #![proptest_config(ProptestConfig::with_cases(80))]

    /// Corrupting the bloom sidecar never changes results or panics — it is a
    /// discardable hint.
    #[test]
    fn corrupt_bloom_sidecar_never_panics(garbage in proptest::collection::vec(any::<u8>(), 0..512)) {
        let dir = tempfile::tempdir().unwrap();
        build_db(dir.path(), &[(b"a".to_vec(), b"1".to_vec()), (b"b".to_vec(), b"2".to_vec())]);
        for sidecar in files_ending(dir.path(), ".sst.bloom") {
            fs::write(&sidecar, &garbage).unwrap();
        }
        let db = Lsm::open(dir.path()).unwrap();
        // Results are unchanged regardless of the corrupt hint.
        prop_assert_eq!(db.get(b"a").unwrap(), Some(b"1".to_vec()));
        prop_assert_eq!(db.get(b"b").unwrap(), Some(b"2".to_vec()));
        prop_assert_eq!(db.get(b"absent").unwrap(), None);
    }
}
