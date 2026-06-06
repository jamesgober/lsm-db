//! Grouped, atomic writes with [`Batch`], and reading data back after reopen.
//!
//! Run with `cargo run --example batch_writes`.

use lsm_db::{Batch, Lsm};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let dir = tempfile::tempdir()?;

    {
        let db = Lsm::open(dir.path())?;

        // Stage a group of changes, then apply them together.
        let mut batch = Batch::new();
        for id in 1..=5u32 {
            batch.put(format!("account:{id}").into_bytes(), b"active");
        }
        batch.put(b"account:3", b"suspended"); // last write to a key wins
        batch.delete(b"account:5");
        println!("applying batch of {} operations", batch.len());
        db.write(batch)?;

        db.flush()?;
    }

    // Reopen and confirm the group is durable.
    let db = Lsm::open(dir.path())?;
    println!("account:1 = {:?}", db.get(b"account:1")?);
    println!("account:3 = {:?}", db.get(b"account:3")?);
    println!("account:5 = {:?}", db.get(b"account:5")?);
    println!("{} live accounts", db.scan(..)?.count());

    Ok(())
}
