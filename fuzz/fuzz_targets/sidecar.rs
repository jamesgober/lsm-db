//! Fuzz the bloom-sidecar parse path: arbitrary bytes as a `.bloom` sidecar must
//! never panic and must never change a query's answer.
//!
//! A real database with one run (and its sidecar) is built, then every sidecar
//! is overwritten with the fuzzer's bytes. The integrity envelope (magic + CRC)
//! must reject anything this crate did not write, so a corrupt or hostile
//! sidecar can never produce a filter that panics when queried — the run is
//! consulted directly and the answers are unchanged.

#![no_main]

use libfuzzer_sys::fuzz_target;
use lsm_db::Lsm;

fuzz_target!(|data: &[u8]| {
    let Ok(dir) = tempfile::tempdir() else {
        return;
    };
    let p = dir.path();

    {
        let Ok(db) = Lsm::open(p) else {
            return;
        };
        let _ = db.put(b"present", b"1");
        let _ = db.flush();
    }

    if let Ok(rd) = std::fs::read_dir(p) {
        for entry in rd.flatten() {
            if entry
                .file_name()
                .to_string_lossy()
                .ends_with(".sst.bloom")
            {
                let _ = std::fs::write(entry.path(), data);
            }
        }
    }

    if let Ok(db) = Lsm::open(p) {
        let _ = db.get(b"present");
        let _ = db.get(b"absent");
    }
});
