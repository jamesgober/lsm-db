//! Engine configuration.
//!
//! [`LsmConfig`] is the Tier-2 tuning surface. The Tier-1 entry point
//! [`Lsm::open`](crate::Lsm::open) uses [`LsmConfig::default`], so most callers
//! never name this type. Reach for it when the default write-buffer size does
//! not suit the workload.

/// Default memtable capacity: 4 MiB of live key and value bytes.
///
/// Chosen as a balance for the foundation release — small enough that flushes
/// stay cheap and predictable, large enough that bulk loads do not flush on
/// every handful of writes. Tune with [`LsmConfig::memtable_capacity`].
pub const DEFAULT_MEMTABLE_CAPACITY: usize = 4 * 1024 * 1024;

/// Tuning parameters for an [`Lsm`](crate::Lsm) engine.
///
/// Construct with [`LsmConfig::new`] (or [`LsmConfig::default`]) and refine with
/// the chained setters, then pass to [`Lsm::open_with`](crate::Lsm::open_with).
///
/// # Examples
///
/// ```
/// use lsm_db::LsmConfig;
///
/// // A 64 KiB write buffer — flushes often, keeps resident memory tiny.
/// let config = LsmConfig::new().memtable_capacity(64 * 1024);
/// assert_eq!(config.memtable_capacity_bytes(), 64 * 1024);
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LsmConfig {
    memtable_capacity: usize,
}

impl LsmConfig {
    /// Start from the default configuration.
    ///
    /// Equivalent to [`LsmConfig::default`]; provided so configuration reads as
    /// a builder chain.
    ///
    /// # Examples
    ///
    /// ```
    /// use lsm_db::LsmConfig;
    /// let config = LsmConfig::new();
    /// assert_eq!(config, LsmConfig::default());
    /// ```
    #[inline]
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the memtable capacity, in bytes of live key and value data.
    ///
    /// When the in-memory write buffer reaches this size, the next write
    /// triggers a flush to disk. A capacity of `0` flushes after every write,
    /// which is useful in tests but rarely otherwise.
    ///
    /// The figure counts key and value bytes only, not per-entry bookkeeping, so
    /// peak resident memory is somewhat higher than the configured number.
    ///
    /// # Examples
    ///
    /// ```
    /// use lsm_db::LsmConfig;
    /// let config = LsmConfig::new().memtable_capacity(1 << 20); // 1 MiB
    /// assert_eq!(config.memtable_capacity_bytes(), 1 << 20);
    /// ```
    #[inline]
    #[must_use]
    pub fn memtable_capacity(mut self, bytes: usize) -> Self {
        self.memtable_capacity = bytes;
        self
    }

    /// The configured memtable capacity, in bytes.
    ///
    /// # Examples
    ///
    /// ```
    /// use lsm_db::LsmConfig;
    /// assert_eq!(
    ///     LsmConfig::default().memtable_capacity_bytes(),
    ///     lsm_db::DEFAULT_MEMTABLE_CAPACITY,
    /// );
    /// ```
    #[inline]
    #[must_use]
    pub fn memtable_capacity_bytes(&self) -> usize {
        self.memtable_capacity
    }
}

impl Default for LsmConfig {
    /// The default configuration: a [`DEFAULT_MEMTABLE_CAPACITY`] write buffer.
    fn default() -> Self {
        LsmConfig {
            memtable_capacity: DEFAULT_MEMTABLE_CAPACITY,
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn test_default_capacity_is_documented_constant() {
        assert_eq!(
            LsmConfig::default().memtable_capacity_bytes(),
            DEFAULT_MEMTABLE_CAPACITY
        );
    }

    #[test]
    fn test_builder_overrides_capacity() {
        let c = LsmConfig::new().memtable_capacity(123);
        assert_eq!(c.memtable_capacity_bytes(), 123);
    }

    #[test]
    fn test_new_equals_default() {
        assert_eq!(LsmConfig::new(), LsmConfig::default());
    }
}
