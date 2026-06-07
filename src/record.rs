//! The value a key maps to within one source.
//!
//! A key's state at any one level — the memtable or a single sorted run — is a
//! [`Record`]: either live data, or a *tombstone* that records a deletion. When
//! several sources disagree, the newest source's record wins; a tombstone there
//! hides any older value, and only the oldest level may drop tombstones during
//! compaction.

/// What a key maps to in one source (the memtable or one run).
///
/// A `Value` is live data. A `Tombstone` records that the key was deleted and
/// must shadow any older value for the same key until the oldest level resolves
/// it away.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Record {
    /// The key maps to this value.
    Value(Vec<u8>),
    /// The key has been deleted; mask any older value.
    Tombstone,
}

impl Record {
    /// The number of value bytes this record holds (zero for a tombstone).
    #[inline]
    pub(crate) fn value_len(&self) -> usize {
        match self {
            Record::Value(v) => v.len(),
            Record::Tombstone => 0,
        }
    }
}
