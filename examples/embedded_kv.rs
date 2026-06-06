//! An embedded key-value store: open, put, get, overwrite, delete.
//!
//! Run with `cargo run --example embedded_kv`.

use lsm_db::Lsm;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // A real application opens a durable directory; this demo uses a temporary
    // one so it leaves nothing behind.
    let dir = tempfile::tempdir()?;
    let db = Lsm::open(dir.path())?;

    db.put(b"config:theme", b"dark")?;
    db.put(b"config:lang", b"en")?;

    println!("theme = {:?}", db.get(b"config:theme")?);
    println!("lang  = {:?}", db.get(b"config:lang")?);

    // Overwrite an existing key.
    db.put(b"config:theme", b"light")?;
    println!("theme = {:?} (after overwrite)", db.get(b"config:theme")?);

    // Delete one key.
    db.delete(b"config:lang")?;
    println!("lang  = {:?} (after delete)", db.get(b"config:lang")?);

    // Force everything to disk; it will be there on the next open.
    db.flush()?;
    println!("flushed; {} live keys", db.scan(..)?.count());

    Ok(())
}
