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

/// Default number of on-disk runs that triggers a background compaction.
///
/// Each flush adds a run, and every point read may have to consult each run, so
/// the run count bounds read amplification. When it reaches this many, the
/// background compactor merges the runs into one. Tune with
/// [`LsmConfig::compaction_trigger`].
pub const DEFAULT_COMPACTION_TRIGGER: usize = 4;

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
    compaction_trigger: usize,
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

    /// Set the number of on-disk runs that triggers a background compaction.
    ///
    /// Reads may consult every run, so this bounds read amplification: the
    /// engine keeps at most roughly this many runs before merging them into one
    /// in the background. Smaller values keep reads fast at the cost of more
    /// compaction work; larger values do the reverse. Values below `2` are
    /// treated as `2`, since merging a single run is pointless.
    ///
    /// # Examples
    ///
    /// ```
    /// use lsm_db::LsmConfig;
    /// let config = LsmConfig::new().compaction_trigger(8);
    /// assert_eq!(config.compaction_trigger_runs(), 8);
    /// ```
    #[inline]
    #[must_use]
    pub fn compaction_trigger(mut self, runs: usize) -> Self {
        self.compaction_trigger = runs.max(2);
        self
    }

    /// The configured compaction trigger, in number of runs.
    ///
    /// # Examples
    ///
    /// ```
    /// use lsm_db::LsmConfig;
    /// assert_eq!(
    ///     LsmConfig::default().compaction_trigger_runs(),
    ///     lsm_db::DEFAULT_COMPACTION_TRIGGER,
    /// );
    /// ```
    #[inline]
    #[must_use]
    pub fn compaction_trigger_runs(&self) -> usize {
        self.compaction_trigger
    }
}

impl Default for LsmConfig {
    /// The default configuration: a [`DEFAULT_MEMTABLE_CAPACITY`] write buffer
    /// and a [`DEFAULT_COMPACTION_TRIGGER`] run threshold.
    fn default() -> Self {
        LsmConfig {
            memtable_capacity: DEFAULT_MEMTABLE_CAPACITY,
            compaction_trigger: DEFAULT_COMPACTION_TRIGGER,
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

    #[test]
    fn test_default_compaction_trigger_is_documented_constant() {
        assert_eq!(
            LsmConfig::default().compaction_trigger_runs(),
            DEFAULT_COMPACTION_TRIGGER
        );
    }

    #[test]
    fn test_compaction_trigger_override() {
        assert_eq!(
            LsmConfig::new()
                .compaction_trigger(8)
                .compaction_trigger_runs(),
            8
        );
    }

    #[test]
    fn test_compaction_trigger_clamped_to_two() {
        assert_eq!(
            LsmConfig::new()
                .compaction_trigger(0)
                .compaction_trigger_runs(),
            2
        );
        assert_eq!(
            LsmConfig::new()
                .compaction_trigger(1)
                .compaction_trigger_runs(),
            2
        );
    }
}
