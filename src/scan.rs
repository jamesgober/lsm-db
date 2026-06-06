//! Range iteration.
//!
//! [`Lsm::scan`](crate::Lsm::scan) merges the in-memory buffer and the on-disk
//! run into one ascending key stream and returns a [`Scan`] over the entries
//! that fall in the requested range.
//!
//! For the foundation release the merge is materialised under the read lock that
//! `scan` takes: the returned [`Scan`] is a consistent point-in-time snapshot of
//! the range, decoupled from later writes, and iterating it never blocks writers.
//! Streaming the merge lazily is a later optimisation (see the roadmap); the
//! snapshot semantics it provides are part of the contract and will not change.

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
