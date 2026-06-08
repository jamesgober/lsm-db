//! Fuzz the sorted-run parse path: arbitrary bytes as a run file must never
//! panic, over-allocate, or read past the input.
//!
//! A manifest names one run whose bytes are the fuzzer's input, so opening the
//! database drives the footer/index/block parser over hostile data. Every length
//! prefix is capped before any payload allocation, so a crafted length cannot
//! force an unbounded read. Open may fail (corruption) or succeed; either way no
//! panic is allowed.

#![no_main]

use libfuzzer_sys::fuzz_target;
use lsm_db::Lsm;

fuzz_target!(|data: &[u8]| {
    let Ok(dir) = tempfile::tempdir() else {
        return;
    };
    let p = dir.path();

    if std::fs::write(p.join("run-0000000000.sst"), data).is_err() {
        return;
    }
    let manifest = "LSMDB-MANIFEST v1\nnext_seq=1\nrun-0000000000.sst\n";
    if std::fs::write(p.join("MANIFEST"), manifest).is_err() {
        return;
    }

    if let Ok(db) = Lsm::open(p) {
        let _ = db.get(b"key");
        let _ = db.get(b"");
        if let Ok(scan) = db.scan(..) {
            let _ = scan.count();
        }
    }
});
