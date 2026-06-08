//! A crash-safe key-value store using the write-ahead log.
//!
//! Requires the `durability` feature, which logs every write to a `wal-db`
//! write-ahead log and `fsync`s it before the call returns. Run with:
//!
//! ```sh
//! cargo run --example durable_store --features durability
//! ```

use lsm_db::Lsm;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let dir = tempfile::tempdir()?;

    // First "session": write some data but never flush it to a run. With the
    // `durability` feature, the writes are still durable — they are in the log.
    {
        let db = Lsm::open(dir.path())?;
        db.put(b"account:alice", b"100")?;
        db.put(b"account:bob", b"250")?;
        // The process ends here without an explicit flush — like a crash.
    }

    // Second "session": reopen the same directory. The log is replayed, so the
    // un-flushed writes are recovered.
    let db = Lsm::open(dir.path())?;
    println!("alice = {:?}", db.get(b"account:alice")?);
    println!("bob   = {:?}", db.get(b"account:bob")?);
    println!(
        "{} accounts recovered from the write-ahead log",
        db.scan(..)?.count()
    );

    Ok(())
}
