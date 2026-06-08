//! Bloom-filtered point reads.
//!
//! Requires the `bloom` feature, which builds a per-run bloom filter (persisted
//! in a sidecar) so a point read can skip any run that cannot contain the key —
//! a large win for negative lookups across many runs. Run with:
//!
//! ```sh
//! cargo run --example bloom_point_reads --features bloom
//! ```

use lsm_db::{Lsm, LsmConfig};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let dir = tempfile::tempdir()?;
    // A high compaction trigger keeps several runs around so the filter has
    // something to skip; in production, compaction keeps the run count low.
    let db = Lsm::open_with(dir.path(), LsmConfig::new().compaction_trigger(64))?;

    // Build several runs of present keys (all even).
    for run in 0..8u32 {
        for i in 0..1_000u32 {
            db.put(
                format!("user:{:06}", i * 2).into_bytes(),
                format!("r{run}").into_bytes(),
            )?;
        }
        db.flush()?;
    }

    // Present key: found.
    println!("user:000010 -> {:?}", db.get(b"user:000010")?);

    // Negative lookups (odd keys never inserted). With the `bloom` feature each
    // run's filter rejects these without reading a single data block.
    let mut misses = 0;
    for i in 0..10_000u32 {
        let key = format!("user:{:06}", i * 2 + 1); // odd keys: all absent
        if db.get(key.as_bytes())?.is_none() {
            misses += 1;
        }
    }
    println!("{misses} negative lookups, each skipping every run via its bloom filter");

    Ok(())
}
