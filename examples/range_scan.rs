//! Range scans over an ordered key space.
//!
//! Keys are stored in sorted order, so a prefix or bounded range can be walked
//! efficiently. Run with `cargo run --example range_scan`.

use lsm_db::Lsm;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let dir = tempfile::tempdir()?;
    let db = Lsm::open(dir.path())?;

    // A simple time series, one event per key.
    db.put(b"event:2026-06-01", b"deploy")?;
    db.put(b"event:2026-06-02", b"scale-up")?;
    db.put(b"event:2026-06-03", b"incident")?;
    db.put(b"event:2026-06-04", b"resolved")?;
    db.put(b"meta:owner", b"platform-team")?;

    // Everything, in key order.
    println!("all entries:");
    for (key, value) in db.scan(..)? {
        println!(
            "  {} = {}",
            String::from_utf8_lossy(&key),
            String::from_utf8_lossy(&value)
        );
    }

    // Just the events from the 2nd to the 4th (half-open: 4th excluded).
    println!("\nevents 06-02 ..= 06-03:");
    let lo = b"event:2026-06-02".to_vec();
    let hi = b"event:2026-06-04".to_vec();
    for (key, value) in db.scan(lo..hi)? {
        println!(
            "  {} = {}",
            String::from_utf8_lossy(&key),
            String::from_utf8_lossy(&value)
        );
    }

    // A prefix scan: everything under "event:".
    println!("\nall events (prefix scan):");
    let count = db.scan(b"event:".to_vec()..b"event;".to_vec())?.count();
    println!("  {count} events");

    Ok(())
}
