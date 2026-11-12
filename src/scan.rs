//! Range iteration.
//!
//! [`Lsm::scan`](crate::Lsm::scan) merges the in-memory buffer and the on-disk
//! runs into one ascending key stream and returns a [`Scan`] over the entries
//! that fall in the requested range.
//!
//! The scan is materialised before it returns: under the read lock it copies
//! the memtable's part of the range and takes a handle on each run, then,
//! with no lock held, it seeks every run to the range's start and merges the
//! range into one vector. The returned [`Scan`] is a consistent point-in-time
//! snapshot of the range, decoupled from later writes, and iterating it never
//! blocks writers. It also holds the whole range in memory at once, so a range
//! larger than memory cannot be scanned in one call; scan such a store in
//! pieces (for example by prefix, or by a key range at a time). Streaming the
//! merge lazily is planned; the snapshot semantics are part of the contract
//! and will not change.

/// An ascending iterator over a key range, returned by
/// [`Lsm::scan`](crate::Lsm::scan).
///
/// Yields `(key, value)` pairs in ascending key order. Deleted keys are already
/// resolved away — a tombstone in the buffer hides the matching on-disk value,
/// and neither appears in the stream.
///
/// # Examples
///
/// ```
/// # fn main() -> Result<(), Box<dyn std::error::Error>> {
/// let dir = tempfile::tempdir()?;
/// let db = lsm_db::Lsm::open(dir.path())?;
/// db.put(b"a", b"1")?;
/// db.put(b"b", b"2")?;
/// db.put(b"c", b"3")?;
///
/// let pairs: Vec<_> = db.scan(b"a".to_vec()..b"c".to_vec())?.collect();
/// assert_eq!(pairs, vec![(b"a".to_vec(), b"1".to_vec()), (b"b".to_vec(), b"2".to_vec())]);
/// # Ok(())
/// # }
/// ```
#[derive(Debug)]
pub struct Scan {
    inner: std::vec::IntoIter<(Vec<u8>, Vec<u8>)>,
}

impl Scan {
    /// Wrap a materialised, already-sorted set of live entries.
    pub(crate) fn new(entries: Vec<(Vec<u8>, Vec<u8>)>) -> Self {
        Scan {
            inner: entries.into_iter(),
        }
    }
}

impl Iterator for Scan {
    type Item = (Vec<u8>, Vec<u8>);

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        self.inner.next()
    }

    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        self.inner.size_hint()
    }
}

impl ExactSizeIterator for Scan {
    #[inline]
    fn len(&self) -> usize {
        self.inner.len()
    }
}

impl DoubleEndedIterator for Scan {
    #[inline]
    fn next_back(&mut self) -> Option<Self::Item> {
        self.inner.next_back()
    }
}
