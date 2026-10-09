//! The sorted-run format as an in-memory codec.
//!
//! `lsm_db::format` reads and writes the engine's run format over byte
//! buffers. It also builds without `std` (`default-features = false,
//! features = ["alloc"]`), so a device can read runs a host produced, or build
//! runs for a host to load. This example does both halves on one machine:
//! it reads a run file the engine flushed, then builds a run in memory.
//!
//! Run with: `cargo run --example run_codec`

use lsm_db::Lsm;
use lsm_db::format::{Entry, RunReader, RunWriter};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // 1. Let the engine flush a run to disk, then read the file as bytes.
    let dir = tempfile::tempdir()?;
    {
        let db = Lsm::open(dir.path())?;
        db.put(b"sensor:1", b"21.5")?;
        db.put(b"sensor:2", b"19.0")?;
        db.delete(b"sensor:3")?;
        db.flush()?;
    }
    let run_path = std::fs::read_dir(dir.path())?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .find(|p| p.extension().is_some_and(|ext| ext == "sst"))
        .ok_or("no run file was flushed")?;
    let bytes = std::fs::read(&run_path)?;

    let run = RunReader::new(&bytes)?;
    println!(
        "{} holds {} entries:",
        run_path.display(),
        run.entry_count()
    );
    for item in &run {
        let (key, entry) = item?;
        match entry {
            Entry::Value(v) => println!(
                "  {} = {}",
                String::from_utf8_lossy(key),
                String::from_utf8_lossy(v)
            ),
            Entry::Tombstone => println!("  {} (deleted)", String::from_utf8_lossy(key)),
        }
    }

    // 2. Build a run in memory. Keys must arrive in ascending order.
    let mut writer = RunWriter::new();
    for i in 0..1_000u32 {
        writer.put(format!("reading:{i:05}"), i.to_le_bytes())?;
    }
    let built = writer.finish();
    let run = RunReader::new(&built)?;
    let hit = run.get(b"reading:00042")?.and_then(Entry::value);
    println!(
        "built a {}-byte run; reading:00042 = {:?}",
        built.len(),
        hit.map(|v| u32::from_le_bytes([v[0], v[1], v[2], v[3]]))
    );

    // Out-of-order input is refused rather than written.
    let mut writer = RunWriter::new();
    writer.put(b"b", b"2")?;
    assert!(writer.put(b"a", b"1").is_err());
    Ok(())
}
