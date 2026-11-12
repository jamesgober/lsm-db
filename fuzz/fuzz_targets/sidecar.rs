//! Fuzz the bloom-sidecar parse path: arbitrary bytes as a `.bloom` sidecar must
//! never make an open or a query panic or hang. (A well-formed filter over
//! other keys can make a point read miss a key; no check can tell, and the
//! target does not assert answers.)
//!
//! A real database with one run (and its sidecar) is built, then every sidecar
//! is overwritten. The first input byte picks how: an odd byte writes the rest
//! as the whole file; an even one wraps the rest in a valid envelope (magic and
//! CRC32C), so the fuzzer's bytes reach the filter decoder itself. The
//! envelope only detects damage; the decoder must check the filter's shape and
//! drop anything unsound, so a hostile sidecar can never produce a filter that
//! panics or stalls when queried.

#![no_main]

use libfuzzer_sys::fuzz_target;
use lsm_db::Lsm;

fuzz_target!(|data: &[u8]| {
    let Some((&mode, rest)) = data.split_first() else {
        return;
    };
    let sidecar = if mode % 2 == 1 {
        rest.to_vec()
    } else {
        let mut bytes = b"LSMBLM01".to_vec();
        bytes.extend_from_slice(&lsm_db::format::crc32c(rest).to_le_bytes());
        bytes.extend_from_slice(rest);
        bytes
    };

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
                let _ = std::fs::write(entry.path(), &sidecar);
            }
        }
    }

    if let Ok(db) = Lsm::open(p) {
        let _ = db.get(b"present");
        let _ = db.get(b"absent");
    }
});
