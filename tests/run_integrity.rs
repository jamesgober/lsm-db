//! The checks an open and a read make of a run file, and of its sidecar.
//!
//! - The footer is covered by a checksum (runs written since 1.4), and its
//!   entry count must fit the blocks in every run, so a damaged count can
//!   never size an allocation.
//! - The header magic is checked.
//! - A run whose blocks overlap (each block with a valid checksum) is
//!   corruption, not a stream of keys out of order.
//! - A bloom sidecar with a valid checksum but an unsound filter is dropped.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::path::{Path, PathBuf};

use lsm_db::format::crc32c;
use lsm_db::{Error, Lsm, LsmConfig};

/// Footer size: entry count, index offset, index length, index CRC, magic.
const FOOTER: usize = 36;
/// Footer check size, just before the index in runs written since 1.4.
const CHECK: usize = 12;

fn run_files(dir: &Path) -> Vec<PathBuf> {
    let mut runs: Vec<PathBuf> = fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("run-") && n.ends_with(".sst"))
        })
        .collect();
    runs.sort();
    runs
}

/// A store in `dir` with one flushed run of a few keys; returns the run's path.
fn one_run(dir: &Path) -> PathBuf {
    let db = Lsm::open_with(dir, LsmConfig::new().compaction_trigger(usize::MAX)).unwrap();
    for i in 0..50u32 {
        db.put(format!("key{i:03}"), [b'v'; 20]).unwrap();
    }
    db.flush().unwrap();
    drop(db);
    run_files(dir).pop().unwrap()
}

fn u64_at(bytes: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap())
}

/// Overwrite the footer's entry count.
fn set_entry_count(bytes: &mut [u8], count: u64) {
    let at = bytes.len() - FOOTER;
    bytes[at..at + 8].copy_from_slice(&count.to_le_bytes());
}

/// The run as a release before 1.4 wrote it: without the footer check. A run
/// that has none is returned as it is.
fn without_footer_check(bytes: &[u8]) -> Vec<u8> {
    let footer_at = bytes.len() - FOOTER;
    let index_offset = u64_at(bytes, footer_at + 8) as usize;
    if index_offset < 8 + CHECK || &bytes[index_offset - CHECK..index_offset - 4] != b"LSMFCK01" {
        return bytes.to_vec();
    }
    let mut out = bytes[..index_offset - CHECK].to_vec();
    out.extend_from_slice(&bytes[index_offset..footer_at]);
    let mut footer = bytes[footer_at..].to_vec();
    footer[8..16].copy_from_slice(&((index_offset - CHECK) as u64).to_le_bytes());
    out.extend_from_slice(&footer);
    out
}

/// A footer entry count of `u64::MAX` (one bit flip in its high bytes does
/// it) used to open without complaint. With `bloom`, the next compaction
/// sized its filter from it and the process aborted on the allocation,
/// again at every restart. The open refuses the run now: in a run written
/// since 1.4 by the footer checksum, in an older one because the count does
/// not fit the run's blocks.
#[test]
fn an_entry_count_the_run_cannot_hold_is_refused_at_open() {
    let dir = tempfile::tempdir().unwrap();
    let run = one_run(dir.path());
    let good = fs::read(&run).unwrap();

    let mut bad = good.clone();
    set_entry_count(&mut bad, u64::MAX);
    fs::write(&run, &bad).unwrap();
    let result = Lsm::open(dir.path());
    assert!(
        matches!(result, Err(Error::Corruption { .. })),
        "{result:?}"
    );

    let mut old_style = without_footer_check(&good);
    set_entry_count(&mut old_style, 1 << 40);
    fs::write(&run, &old_style).unwrap();
    let result = Lsm::open(dir.path());
    assert!(
        matches!(result, Err(Error::Corruption { .. })),
        "{result:?}"
    );
}

/// Runs written before 1.4 (no footer check) still open and read.
#[test]
fn a_run_without_the_footer_check_still_opens() {
    let dir = tempfile::tempdir().unwrap();
    let run = one_run(dir.path());
    let old_style = without_footer_check(&fs::read(&run).unwrap());
    fs::write(&run, &old_style).unwrap();
    let db = Lsm::open(dir.path()).unwrap();
    assert_eq!(db.scan(..).unwrap().count(), 50);
    assert_eq!(db.get(b"key007").unwrap(), Some(vec![b'v'; 20]));
}

/// The header magic was not checked when a run opened.
#[test]
fn a_run_with_a_bad_header_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let run = one_run(dir.path());
    let mut bytes = fs::read(&run).unwrap();
    bytes[..8].copy_from_slice(b"NOTARUN!");
    fs::write(&run, &bytes).unwrap();
    let result = Lsm::open(dir.path());
    assert!(
        matches!(result, Err(Error::Corruption { .. })),
        "{result:?}"
    );
}

fn entry(out: &mut Vec<u8>, key: &[u8], value: &[u8]) {
    out.extend_from_slice(&(key.len() as u32).to_le_bytes());
    out.extend_from_slice(key);
    out.push(0);
    out.extend_from_slice(&(value.len() as u32).to_le_bytes());
    out.extend_from_slice(value);
}

/// A run whose two blocks overlap: `a, c` then `b, d`. Each block is in
/// order and has a valid checksum, and the index keys (`c`, `d`) increase.
fn overlapping_run() -> Vec<u8> {
    let mut out = b"LSMTBL01".to_vec();
    let mut blocks = Vec::new();
    for keys in [[b"a", b"c"], [b"b", b"d"]] {
        let mut block = Vec::new();
        for k in keys {
            entry(&mut block, k, b"v");
        }
        blocks.push((
            out.len() as u64,
            block.len() as u32,
            crc32c(&block),
            keys[1],
        ));
        out.extend_from_slice(&block);
    }
    let index_offset = out.len() as u64;
    let mut index = Vec::new();
    for (offset, len, crc, last) in &blocks {
        index.extend_from_slice(&(last.len() as u32).to_le_bytes());
        index.extend_from_slice(*last);
        index.extend_from_slice(&offset.to_le_bytes());
        index.extend_from_slice(&len.to_le_bytes());
        index.extend_from_slice(&crc.to_le_bytes());
    }
    out.extend_from_slice(&index);
    out.extend_from_slice(&4u64.to_le_bytes());
    out.extend_from_slice(&index_offset.to_le_bytes());
    out.extend_from_slice(&(index.len() as u64).to_le_bytes());
    out.extend_from_slice(&crc32c(&index).to_le_bytes());
    out.extend_from_slice(b"LSMTBL01");
    out
}

/// A run with overlapping blocks made a scan (and a compaction, which then
/// wrote a run that fails its own checks) emit keys out of order. Reading
/// it is a corruption error now.
#[test]
fn a_run_whose_blocks_overlap_is_corruption() {
    let dir = tempfile::tempdir().unwrap();
    drop(Lsm::open(dir.path()).unwrap());
    fs::write(dir.path().join("run-0000000000.sst"), overlapping_run()).unwrap();
    fs::write(
        dir.path().join("MANIFEST"),
        "LSMDB-MANIFEST v1\nnext_seq=1\nrun-0000000000.sst\n",
    )
    .unwrap();
    let db = Lsm::open(dir.path()).unwrap();
    match db.scan(..) {
        Err(Error::Corruption { .. }) => {}
        Err(other) => panic!("expected corruption, got {other:?}"),
        Ok(scan) => {
            let keys: Vec<Vec<u8>> = scan.map(|(k, _)| k).collect();
            panic!("the scan returned {keys:?}");
        }
    }
}

/// A sidecar with a valid checksum over an unsound filter: no words for
/// 2^40 bits. It used to decode, and the first point read indexed past the
/// end of the filter and panicked. It is dropped now, and reads answer from
/// the run.
#[cfg(feature = "bloom")]
#[test]
fn a_crafted_bloom_sidecar_is_dropped_not_trusted() {
    fn leb(mut v: u64, out: &mut Vec<u8>) {
        loop {
            let byte = (v & 0x7F) as u8;
            v >>= 7;
            if v == 0 {
                out.push(byte);
                return;
            }
            out.push(byte | 0x80);
        }
    }
    let dir = tempfile::tempdir().unwrap();
    let run = one_run(dir.path());
    let mut payload = Vec::new();
    leb(0, &mut payload); // no words
    leb(1 << 40, &mut payload); // bits
    leb(7, &mut payload); // hashes
    let mut sidecar = b"LSMBLM01".to_vec();
    sidecar.extend_from_slice(&crc32c(&payload).to_le_bytes());
    sidecar.extend_from_slice(&payload);
    let mut path = run.into_os_string();
    path.push(".bloom");
    fs::write(&path, &sidecar).unwrap();

    let db = Lsm::open(dir.path()).unwrap();
    assert_eq!(db.get(b"key001").unwrap(), Some(vec![b'v'; 20]));
    assert_eq!(db.get(b"absent").unwrap(), None);
}
