//! Per-run bloom filters.
//!
//! A point lookup that misses the memtable has to consult the on-disk runs. A
//! bloom filter over a run's keys lets the engine answer "this run definitely
//! does not contain this key" without reading a single data block, so a negative
//! lookup touches far fewer runs. The filter can have false positives (it may
//! say "maybe" for an absent key) but never false negatives, so skipping a run
//! it rejects is always safe.
//!
//! The on-disk run format is frozen for the 1.x series, so the filter is not
//! embedded in the run file. It lives in a **sidecar** next to it
//! (`<run>.bloom`), written when the run is created and loaded when it is
//! reopened. A sidecar is a pure acceleration hint: if it is missing or
//! unreadable the engine simply consults the run directly, with correct (only
//! slower) results.
//!
//! Everything here presents the same surface whether or not the `bloom` feature
//! is enabled; with it off, [`RunFilter`] is a zero-sized no-op the engine calls
//! unconditionally, so the non-bloom path pays nothing.

use std::path::{Path, PathBuf};

/// The sidecar path for a run file: the run path with `.bloom` appended.
///
/// Defined unconditionally so run-file cleanup can remove a stale sidecar
/// regardless of whether the `bloom` feature is enabled.
pub(crate) fn sidecar_path(run_path: &Path) -> PathBuf {
    let mut name = run_path.as_os_str().to_os_string();
    name.push(".bloom");
    PathBuf::from(name)
}

#[cfg(feature = "bloom")]
pub(crate) use enabled::{RunFilter, builder};

#[cfg(not(feature = "bloom"))]
pub(crate) use disabled::{RunFilter, builder};

#[cfg(not(feature = "bloom"))]
mod disabled {
    use std::path::Path;

    use crate::error::Result;

    /// No-op run filter used when the `bloom` feature is disabled.
    #[derive(Debug)]
    pub(crate) struct RunFilter;

    /// No-op builder used when the `bloom` feature is disabled.
    #[derive(Debug)]
    pub(crate) struct RunFilterBuilder;

    /// Start a no-op builder.
    #[inline]
    pub(crate) fn builder(_capacity: usize) -> RunFilterBuilder {
        RunFilterBuilder
    }

    impl RunFilterBuilder {
        #[inline]
        pub(crate) fn add(&mut self, _key: &[u8]) {}

        #[inline]
        pub(crate) fn finish(self) -> Option<RunFilter> {
            None
        }
    }

    impl RunFilter {
        #[inline]
        pub(crate) fn load(_run_path: &Path) -> Result<Option<RunFilter>> {
            Ok(None)
        }

        #[inline]
        pub(crate) fn write_sidecar(&self, _run_path: &Path) -> Result<()> {
            Ok(())
        }

        #[inline]
        pub(crate) fn might_contain(&self, _key: &[u8]) -> bool {
            true
        }
    }
}

#[cfg(feature = "bloom")]
mod enabled {
    use std::fs;
    use std::path::Path;

    use bloom_lib::BloomFilter;

    use super::sidecar_path;
    use crate::error::{Error, Result};

    /// Target false-positive rate for a run filter. 1% keeps the filter small
    /// (about 9.6 bits per key) while rejecting ~99% of absent keys before any
    /// block read — the rest fall through to a normal, still-correct lookup.
    const FALSE_POSITIVE_RATE: f64 = 0.01;

    /// A bloom filter over the keys of one sorted run.
    #[derive(Debug)]
    pub(crate) struct RunFilter {
        filter: BloomFilter<[u8]>,
    }

    /// Accumulates keys into a filter as a run is written.
    #[derive(Debug)]
    pub(crate) struct RunFilterBuilder {
        /// `None` if the run is empty or the filter could not be sized, in which
        /// case the run simply gets no filter (always-consult behaviour).
        filter: Option<BloomFilter<[u8]>>,
    }

    /// Start a builder sized for `capacity` keys.
    ///
    /// `capacity` may be an over-estimate (compaction sizes from the sum of its
    /// inputs, before dedup); over-sizing only lowers the false-positive rate.
    pub(crate) fn builder(capacity: usize) -> RunFilterBuilder {
        // A zero-key run gets no filter. `BloomFilter::new` also rejects a zero
        // capacity, so guard it here.
        let filter = if capacity == 0 {
            None
        } else {
            BloomFilter::new(capacity, FALSE_POSITIVE_RATE).ok()
        };
        RunFilterBuilder { filter }
    }

    impl RunFilterBuilder {
        /// Record a key. Keys must be added in the order the run is written;
        /// order does not affect the filter, only completeness does.
        #[inline]
        pub(crate) fn add(&mut self, key: &[u8]) {
            if let Some(filter) = self.filter.as_mut() {
                let _ = filter.insert(key);
            }
        }

        /// Finish building. Returns `None` for an empty run.
        pub(crate) fn finish(self) -> Option<RunFilter> {
            self.filter.map(|filter| RunFilter { filter })
        }
    }

    impl RunFilter {
        /// Load the filter for the run at `run_path` from its sidecar.
        ///
        /// Returns `Ok(None)` when the sidecar is absent or unreadable as a
        /// filter — both are non-fatal: the run is simply consulted directly. A
        /// genuine I/O failure (for example a permission error) is propagated.
        pub(crate) fn load(run_path: &Path) -> Result<Option<RunFilter>> {
            let path = sidecar_path(run_path);
            let bytes = match fs::read(&path) {
                Ok(bytes) => bytes,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                Err(e) => return Err(Error::io("read bloom sidecar", e)),
            };
            match postcard::from_bytes::<BloomFilter<[u8]>>(&bytes) {
                Ok(filter) => Ok(Some(RunFilter { filter })),
                // A corrupt sidecar is a discardable hint, not data loss.
                Err(_) => {
                    let _ = fs::remove_file(&path);
                    Ok(None)
                }
            }
        }

        /// Write the filter to the sidecar for the run at `run_path`, atomically
        /// (temporary file, then rename).
        pub(crate) fn write_sidecar(&self, run_path: &Path) -> Result<()> {
            let path = sidecar_path(run_path);
            let bytes = postcard::to_allocvec(&self.filter)
                .map_err(|_| Error::corruption("failed to encode bloom sidecar"))?;
            let mut tmp = path.clone().into_os_string();
            tmp.push(".tmp");
            let tmp = std::path::PathBuf::from(tmp);
            fs::write(&tmp, &bytes).map_err(|e| Error::io("write bloom sidecar", e))?;
            fs::rename(&tmp, &path).map_err(|e| Error::io("install bloom sidecar", e))?;
            Ok(())
        }

        /// Whether the run *might* contain `key`. `false` is definitive (the key
        /// is absent); `true` means consult the run.
        #[inline]
        pub(crate) fn might_contain(&self, key: &[u8]) -> bool {
            self.filter.contains(key)
        }
    }

    #[cfg(test)]
    #[allow(clippy::unwrap_used, clippy::expect_used)]
    mod tests {
        use super::*;

        fn build(keys: &[&[u8]]) -> RunFilter {
            let mut b = builder(keys.len());
            for k in keys {
                b.add(k);
            }
            b.finish().expect("non-empty")
        }

        #[test]
        fn test_no_false_negatives() {
            let f = build(&[b"alpha", b"beta", b"gamma"]);
            assert!(f.might_contain(b"alpha"));
            assert!(f.might_contain(b"beta"));
            assert!(f.might_contain(b"gamma"));
        }

        #[test]
        fn test_rejects_absent_keys() {
            // 1000 present keys; none of a disjoint 1000 should mostly pass.
            let present: Vec<Vec<u8>> = (0..1000).map(|i| format!("k{i}").into_bytes()).collect();
            let refs: Vec<&[u8]> = present.iter().map(Vec::as_slice).collect();
            let f = build(&refs);
            let false_positives = (0..1000)
                .filter(|i| f.might_contain(format!("absent{i}").as_bytes()))
                .count();
            // ~1% target; allow generous slack for the probabilistic bound.
            assert!(
                false_positives < 50,
                "too many false positives: {false_positives}"
            );
        }

        #[test]
        fn test_empty_builder_yields_no_filter() {
            assert!(builder(0).finish().is_none());
        }

        #[test]
        fn test_sidecar_roundtrip() {
            let dir = tempfile::tempdir().unwrap();
            let run = dir.path().join("run-0000000001.sst");
            std::fs::write(&run, b"placeholder run file").unwrap();

            let f = build(&[b"one", b"two", b"three"]);
            f.write_sidecar(&run).unwrap();
            assert!(sidecar_path(&run).exists());

            let loaded = RunFilter::load(&run).unwrap().expect("sidecar present");
            assert!(loaded.might_contain(b"one"));
            assert!(loaded.might_contain(b"three"));
        }

        #[test]
        fn test_load_missing_sidecar_is_none() {
            let dir = tempfile::tempdir().unwrap();
            let run = dir.path().join("run.sst");
            assert!(RunFilter::load(&run).unwrap().is_none());
        }

        #[test]
        fn test_load_corrupt_sidecar_is_none_and_removed() {
            let dir = tempfile::tempdir().unwrap();
            let run = dir.path().join("run.sst");
            std::fs::write(sidecar_path(&run), b"not a valid filter").unwrap();
            assert!(RunFilter::load(&run).unwrap().is_none());
            assert!(
                !sidecar_path(&run).exists(),
                "corrupt sidecar should be removed"
            );
        }
    }
}
