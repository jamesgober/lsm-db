//! A handle dropped without `flush` or `close` keeps what was written.

use lsm_db::Lsm;

#[test]
fn test_drop_without_flush_keeps_writes() {
    let dir = tempfile::tempdir().unwrap();
    {
        let db = Lsm::open(dir.path()).unwrap();
        db.put(b"k", b"v").unwrap();
        db.delete(b"gone").unwrap();
        // Dropped here: no flush, no close.
    }
    let db = Lsm::open(dir.path()).unwrap();
    assert_eq!(db.get(b"k").unwrap(), Some(b"v".to_vec()));
}
