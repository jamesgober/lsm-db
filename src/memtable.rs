//! The in-memory write buffer.
//!
//! Every write lands first in a [`MemTable`]: a sorted map from key to the most
//! recent value, or to a *tombstone* that masks any older value for that key in
//! an on-disk run. The map is ordered, so flushing it produces a sorted run in
//! a single pass, and range scans walk it in key order without a sort.
//!
//! The table tracks an approximate live byte size so the engine can decide when
//! it is full and should be flushed. The figure counts the key and value bytes
//! actually held; it deliberately ignores per-entry map overhead, so it is a
//! lower bound on resident memory, not an exact accounting.

use std::collections::{BTreeMap, btree_map};

use crate::record::Record;

/// A sorted, in-memory buffer of the most recent write per key.
#[derive(Debug, Default)]
pub(crate) struct MemTable {
    entries: BTreeMap<Vec<u8>, Record>,
    /// Approximate live size: the sum of key and value byte lengths currently
    /// held. Updated on every insert so the engine can check it in O(1).
    approx_size: usize,
}

impl MemTable {
    /// Create an empty buffer.
    #[inline]
    pub(crate) fn new() -> Self {
        MemTable {
            entries: BTreeMap::new(),
            approx_size: 0,
        }
    }

    /// Apply a record (value or tombstone) for `key`, replacing any previous
    /// record. This is the single mutation entry point: writes build the
    /// [`Record`] once and hand the same value to the log and the buffer.
    pub(crate) fn apply(&mut self, key: Vec<u8>, record: Record) {
        self.insert(key, record);
    }

    /// Insert a record, keeping [`approx_size`](Self::approx_size) consistent.
    ///
    /// The size delta accounts for the previous record (if any) being replaced:
    /// re-putting a key does not double-count its key bytes.
    fn insert(&mut self, key: Vec<u8>, record: Record) {
        let added = key.len() + record.value_len();
        match self.entries.entry(key) {
            btree_map::Entry::Occupied(mut slot) => {
                // The key bytes are already counted; swap only the value cost.
                self.approx_size = self.approx_size - slot.get().value_len() + record.value_len();
                let _ = slot.insert(record);
            }
            btree_map::Entry::Vacant(slot) => {
                self.approx_size += added;
                let _ = slot.insert(record);
            }
        }
    }

    /// Look up the current record for `key`, if the buffer holds one.
    ///
    /// A returned [`Record::Tombstone`] means the key was deleted and the
    /// caller must not fall through to an on-disk run.
    #[inline]
    pub(crate) fn get(&self, key: &[u8]) -> Option<&Record> {
        self.entries.get(key)
    }

    /// The approximate live size in bytes (sum of key and value lengths held).
    #[inline]
    pub(crate) fn approx_size(&self) -> usize {
        self.approx_size
    }

    /// Whether the buffer holds no records at all.
    #[inline]
    pub(crate) fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Iterate the buffered records in ascending key order.
    #[inline]
    pub(crate) fn iter(&self) -> btree_map::Iter<'_, Vec<u8>, Record> {
        self.entries.iter()
    }

    /// Replace the buffer with an empty one and return the old contents,
    /// resetting the size counter. Used at flush time to take the buffer in one
    /// move without copying every entry.
    pub(crate) fn take(&mut self) -> BTreeMap<Vec<u8>, Record> {
        self.approx_size = 0;
        std::mem::take(&mut self.entries)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    impl MemTable {
        fn put(&mut self, key: Vec<u8>, value: Vec<u8>) {
            self.apply(key, Record::Value(value));
        }
        fn delete(&mut self, key: Vec<u8>) {
            self.apply(key, Record::Tombstone);
        }
    }

    #[test]
    fn test_put_then_get_returns_value() {
        let mut m = MemTable::new();
        m.put(b"a".to_vec(), b"1".to_vec());
        assert_eq!(m.get(b"a"), Some(&Record::Value(b"1".to_vec())));
    }

    #[test]
    fn test_delete_records_tombstone() {
        let mut m = MemTable::new();
        m.put(b"a".to_vec(), b"1".to_vec());
        m.delete(b"a".to_vec());
        assert_eq!(m.get(b"a"), Some(&Record::Tombstone));
    }

    #[test]
    fn test_missing_key_returns_none() {
        let m = MemTable::new();
        assert_eq!(m.get(b"absent"), None);
    }

    #[test]
    fn test_approx_size_tracks_inserts() {
        let mut m = MemTable::new();
        assert_eq!(m.approx_size(), 0);
        m.put(b"ab".to_vec(), b"xyz".to_vec()); // 2 + 3
        assert_eq!(m.approx_size(), 5);
        m.put(b"k".to_vec(), b"v".to_vec()); // + 1 + 1
        assert_eq!(m.approx_size(), 7);
    }

    #[test]
    fn test_approx_size_replaces_value_not_key() {
        let mut m = MemTable::new();
        m.put(b"k".to_vec(), b"short".to_vec()); // 1 + 5 = 6
        assert_eq!(m.approx_size(), 6);
        m.put(b"k".to_vec(), b"longer!".to_vec()); // key already counted: 1 + 7 = 8
        assert_eq!(m.approx_size(), 8);
        m.delete(b"k".to_vec()); // value drops to 0: 1 + 0 = 1
        assert_eq!(m.approx_size(), 1);
    }

    #[test]
    fn test_iter_is_sorted() {
        let mut m = MemTable::new();
        m.put(b"c".to_vec(), b"3".to_vec());
        m.put(b"a".to_vec(), b"1".to_vec());
        m.put(b"b".to_vec(), b"2".to_vec());
        let keys: Vec<&[u8]> = m.iter().map(|(k, _)| k.as_slice()).collect();
        assert_eq!(keys, vec![b"a".as_slice(), b"b", b"c"]);
    }

    #[test]
    fn test_take_empties_and_resets_size() {
        let mut m = MemTable::new();
        m.put(b"a".to_vec(), b"1".to_vec());
        let taken = m.take();
        assert_eq!(taken.len(), 1);
        assert!(m.is_empty());
        assert_eq!(m.approx_size(), 0);
    }
}
