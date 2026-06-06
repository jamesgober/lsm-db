//! Grouped writes.
//!
//! A [`Batch`] collects a sequence of puts and deletes and hands them to
//! [`Lsm::write`](crate::Lsm::write), which applies the whole group under a
//! single lock acquisition. That makes the group atomic with respect to
//! concurrent readers — a reader sees either none of the batch or all of it,
//! never a half-applied state — and amortises the per-write locking cost across
//! the group.

/// One buffered operation in a [`Batch`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Op {
    /// Set `key` to a value.
    Put(Vec<u8>),
    /// Delete `key`.
    Delete,
}

/// An ordered group of writes applied together.
///
/// Operations are recorded in call order and replayed in that order when the
/// batch is written, so a later operation on a key overrides an earlier one.
///
/// # Examples
///
/// ```
/// use lsm_db::Batch;
///
/// let mut batch = Batch::new();
/// batch.put(b"alpha", b"1");
/// batch.put(b"beta", b"2");
/// batch.delete(b"gamma");
/// assert_eq!(batch.len(), 3);
/// ```
#[derive(Debug, Default, Clone)]
pub struct Batch {
    ops: Vec<(Vec<u8>, Op)>,
}

impl Batch {
    /// Create an empty batch.
    ///
    /// # Examples
    ///
    /// ```
    /// use lsm_db::Batch;
    /// let batch = Batch::new();
    /// assert!(batch.is_empty());
    /// ```
    #[inline]
    #[must_use]
    pub fn new() -> Self {
        Batch { ops: Vec::new() }
    }

    /// Queue setting `key` to `value`.
    ///
    /// Both are copied into the batch, so the caller's buffers are free to be
    /// reused immediately.
    ///
    /// # Examples
    ///
    /// ```
    /// use lsm_db::Batch;
    /// let mut batch = Batch::new();
    /// batch.put(b"key", b"value");
    /// assert_eq!(batch.len(), 1);
    /// ```
    pub fn put(&mut self, key: impl AsRef<[u8]>, value: impl AsRef<[u8]>) {
        self.ops
            .push((key.as_ref().to_vec(), Op::Put(value.as_ref().to_vec())));
    }

    /// Queue deleting `key`.
    ///
    /// # Examples
    ///
    /// ```
    /// use lsm_db::Batch;
    /// let mut batch = Batch::new();
    /// batch.delete(b"key");
    /// assert_eq!(batch.len(), 1);
    /// ```
    pub fn delete(&mut self, key: impl AsRef<[u8]>) {
        self.ops.push((key.as_ref().to_vec(), Op::Delete));
    }

    /// The number of queued operations.
    #[inline]
    #[must_use]
    pub fn len(&self) -> usize {
        self.ops.len()
    }

    /// Whether the batch has no queued operations.
    #[inline]
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.ops.is_empty()
    }

    /// Consume the batch, yielding its operations in call order.
    pub(crate) fn into_ops(self) -> Vec<(Vec<u8>, Op)> {
        self.ops
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn test_new_batch_is_empty() {
        let b = Batch::new();
        assert!(b.is_empty());
        assert_eq!(b.len(), 0);
    }

    #[test]
    fn test_records_operations_in_order() {
        let mut b = Batch::new();
        b.put(b"a", b"1");
        b.delete(b"b");
        let ops = b.into_ops();
        assert_eq!(ops[0], (b"a".to_vec(), Op::Put(b"1".to_vec())));
        assert_eq!(ops[1], (b"b".to_vec(), Op::Delete));
    }

    #[test]
    fn test_accepts_vec_and_slice_keys() {
        let mut b = Batch::new();
        b.put(vec![1u8, 2, 3], vec![4u8]);
        b.put([9u8, 8].as_slice(), [7u8].as_slice());
        assert_eq!(b.len(), 2);
    }
}
