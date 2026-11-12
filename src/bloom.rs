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
//! A sidecar is checked, not trusted. Its CRC32C detects damage, but anyone
//! who can write the file can also compute the checksum, so the decoded
//! filter's shape is checked as well (its bit count, word count, and hash
//! count agree and are in range) and a filter that fails is dropped. What no
//! check can catch is a well-formed filter built over other keys: it makes
//! point reads miss keys that the run holds (scans still find them). The
//! sidecar has no key; keep the database directory writable only by its
//! owner.
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

/// The temporary file a sidecar is written to before it is renamed into
/// place.
pub(crate) fn sidecar_tmp_path(run_path: &Path) -> PathBuf {
    let mut name = sidecar_path(run_path).into_os_string();
    name.push(".tmp");
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

    use super::{sidecar_path, sidecar_tmp_path};
    use crate::error::{Error, Result};

    /// Target false-positive rate for a run filter. 1% keeps the filter small
    /// (about 9.6 bits per key) while rejecting ~99% of absent keys before any
    /// block read — the rest fall through to a normal, still-correct lookup.
    const FALSE_POSITIVE_RATE: f64 = 0.01;

    /// Magic prefix of a sidecar file: identifies the format and guards against
    /// feeding unrelated bytes to the deserializer.
    const SIDECAR_MAGIC: &[u8; 8] = b"LSMBLM01";

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

    /// Bits per key at [`FALSE_POSITIVE_RATE`] (`-ln(0.01) / ln(2)^2` is
    /// 9.6), rounded up. Used only to check that a filter's memory can be had
    /// before asking for it.
    const BITS_PER_KEY: u64 = 10;

    /// Largest hash count a sidecar may carry. The engine's filters use 7.
    const MAX_HASHES: u32 = 32;

    /// Start a builder sized for `capacity` keys.
    ///
    /// `capacity` may be an over-estimate (compaction sizes from the sum of its
    /// inputs, before dedup); over-sizing only lowers the false-positive rate.
    /// When the filter's memory cannot be allocated the run gets no filter,
    /// which costs reads speed, not correctness. bloom-lib allocates its bit
    /// array with an allocation that aborts the process when it fails, so the
    /// memory is first reserved with one that reports the failure.
    pub(crate) fn builder(capacity: usize) -> RunFilterBuilder {
        // A zero-key run gets no filter. `BloomFilter::new` also rejects a zero
        // capacity, so guard it here.
        let filter = if capacity == 0 || !can_allocate(capacity) {
            None
        } else {
            BloomFilter::new(capacity, FALSE_POSITIVE_RATE).ok()
        };
        RunFilterBuilder { filter }
    }

    /// Whether the bit array for `capacity` keys can be allocated now.
    fn can_allocate(capacity: usize) -> bool {
        let Some(words) = u64::try_from(capacity)
            .ok()
            .and_then(|n| n.checked_mul(BITS_PER_KEY))
            .map(|bits| bits.div_ceil(64))
            .and_then(|w| usize::try_from(w).ok())
        else {
            return false;
        };
        let mut probe: Vec<u64> = Vec::new();
        probe.try_reserve_exact(words).is_ok()
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
        /// Returns `Ok(None)` when the sidecar is absent or fails its checks:
        /// both are non-fatal, and the run is simply consulted directly. A
        /// genuine I/O failure (for example a permission error) is propagated.
        ///
        /// The envelope (magic + CRC32C over the payload) detects damage; it
        /// is not a security boundary, because anyone who can write the file
        /// can compute the checksum. So the payload's shape is read before it
        /// is decoded and checked after: the word count matches the bit count
        /// and the hash count is in range, which is what makes a query of the
        /// filter safe (no index outside its bits, no unbounded hashing).
        pub(crate) fn load(run_path: &Path) -> Result<Option<RunFilter>> {
            let path = sidecar_path(run_path);
            let bytes = match fs::read(&path) {
                Ok(bytes) => bytes,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                Err(e) => return Err(Error::io("read bloom sidecar", e)),
            };
            match Self::decode(&bytes) {
                Some(filter) => Ok(Some(RunFilter { filter })),
                // A corrupt sidecar is a discardable hint, not data loss.
                None => {
                    let _ = fs::remove_file(&path);
                    Ok(None)
                }
            }
        }

        /// Parse a sidecar envelope, returning the filter only if the magic and
        /// checksum match and the filter's shape is sound. Any inconsistency
        /// yields `None`.
        fn decode(bytes: &[u8]) -> Option<BloomFilter<[u8]>> {
            // magic (8) + crc (4) + payload.
            if bytes.len() < 12 || &bytes[0..8] != SIDECAR_MAGIC {
                return None;
            }
            let crc = u32::from_le_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]);
            let payload = &bytes[12..];
            if crate::crc::crc32c(payload) != crc {
                return None;
            }
            let shape = Shape::read(payload)?;
            if !shape.is_sound() {
                return None;
            }
            let filter = postcard::from_bytes::<BloomFilter<[u8]>>(payload).ok()?;
            // The decoder read the same fields; check that it agrees.
            (filter.num_bits() == shape.num_bits && filter.num_hashes() == shape.num_hashes)
                .then_some(filter)
        }

        /// Write the filter to the sidecar for the run at `run_path`, atomically
        /// (temporary file, then rename), wrapped in its integrity envelope.
        pub(crate) fn write_sidecar(&self, run_path: &Path) -> Result<()> {
            let path = sidecar_path(run_path);
            let bytes = self.encode()?;

            let tmp = sidecar_tmp_path(run_path);
            fs::write(&tmp, &bytes).map_err(|e| Error::io("write bloom sidecar", e))?;
            fs::rename(&tmp, &path).map_err(|e| Error::io("install bloom sidecar", e))?;
            Ok(())
        }

        /// The sidecar's bytes: magic, CRC32C of the payload, and the payload
        /// (the filter as postcard encodes it). The bytes are frozen for 1.x:
        /// `tests/data/bloom-sidecar-v1.bin` holds a sidecar written by 1.2.0,
        /// which every later build must decode and encode identically.
        fn encode(&self) -> Result<Vec<u8>> {
            let payload = postcard::to_allocvec(&self.filter)
                .map_err(|_| Error::corruption("failed to encode bloom sidecar"))?;
            let mut bytes = Vec::with_capacity(12 + payload.len());
            bytes.extend_from_slice(SIDECAR_MAGIC);
            bytes.extend_from_slice(&crate::crc::crc32c(&payload).to_le_bytes());
            bytes.extend_from_slice(&payload);
            Ok(bytes)
        }

        /// Whether the run *might* contain `key`. `false` is definitive (the key
        /// is absent); `true` means consult the run.
        #[inline]
        pub(crate) fn might_contain(&self, key: &[u8]) -> bool {
            self.filter.contains(key)
        }
    }

    /// The shape of a filter as its sidecar payload records it.
    ///
    /// The payload is the postcard encoding of bloom-lib 1.0's
    /// `BloomFilter`: the bit set's words (a length, then each `u64`), the
    /// bit count, then the hash count, every integer a LEB128 varint. The
    /// golden sidecar test pins that layout.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    struct Shape {
        words: u64,
        num_bits: u64,
        num_hashes: u32,
    }

    impl Shape {
        /// Walk the payload without decoding it into memory. `None` unless it
        /// is exactly one filter, with nothing after it.
        fn read(payload: &[u8]) -> Option<Shape> {
            let mut pos = 0usize;
            let words = varint(payload, &mut pos)?;
            for _ in 0..words {
                let _ = varint(payload, &mut pos)?;
            }
            let num_bits = varint(payload, &mut pos)?;
            let num_hashes = u32::try_from(varint(payload, &mut pos)?).ok()?;
            (pos == payload.len()).then_some(Shape {
                words,
                num_bits,
                num_hashes,
            })
        }

        /// The shape every query relies on: at least one bit, exactly enough
        /// words for the bits, and a bounded hash count.
        fn is_sound(self) -> bool {
            self.num_bits > 0
                && self.words == self.num_bits.div_ceil(64)
                && (1..=MAX_HASHES).contains(&self.num_hashes)
        }
    }

    /// Read one LEB128 varint of at most ten bytes (a `u64`).
    fn varint(bytes: &[u8], pos: &mut usize) -> Option<u64> {
        let mut value = 0u64;
        for shift in (0..70).step_by(7) {
            let byte = *bytes.get(*pos)?;
            *pos += 1;
            let low = u64::from(byte & 0x7F);
            if shift == 63 && low > 1 {
                return None; // past 64 bits
            }
            value |= low << shift;
            if byte & 0x80 == 0 {
                return Some(value);
            }
        }
        None
    }

    #[cfg(test)]
    #[allow(clippy::unwrap_used, clippy::expect_used)]
    mod tests {
        use super::*;

        /// A sidecar for `payload`, with a valid envelope.
        fn envelope(payload: &[u8]) -> Vec<u8> {
            let mut bytes = SIDECAR_MAGIC.to_vec();
            bytes.extend_from_slice(&crate::crc::crc32c(payload).to_le_bytes());
            bytes.extend_from_slice(payload);
            bytes
        }

        /// LEB128 varint encoding, as postcard writes it.
        fn leb(mut v: u64, out: &mut Vec<u8>) {
            loop {
                let byte = (v & 0x7F) as u8;
                v >>= 7;
                if v == 0 {
                    out.push(byte);
                    return;
                }
                out.push(byte | 0x80);
            }
        }

        fn payload(words: &[u64], num_bits: u64, num_hashes: u64) -> Vec<u8> {
            let mut out = Vec::new();
            leb(words.len() as u64, &mut out);
            for &w in words {
                leb(w, &mut out);
            }
            leb(num_bits, &mut out);
            leb(num_hashes, &mut out);
            out
        }

        /// A sidecar whose checksum is right but whose filter has no words
        /// for its bits used to decode, and the first query indexed past the
        /// end of the bit set and panicked. One with a huge hash count made
        /// every query hash billions of times. Both are dropped now; a sound
        /// one still decodes.
        #[test]
        fn test_a_crafted_sidecar_with_a_valid_crc_is_dropped() {
            let empty_words = envelope(&payload(&[], 1 << 40, 7));
            assert!(RunFilter::decode(&empty_words).is_none());
            let many_hashes = envelope(&payload(&[u64::MAX; 2], 128, u64::from(u32::MAX)));
            assert!(RunFilter::decode(&many_hashes).is_none());
            let no_bits = envelope(&payload(&[], 0, 7));
            assert!(RunFilter::decode(&no_bits).is_none());
            let trailing = {
                let mut p = payload(&[0; 2], 128, 7);
                p.push(0);
                envelope(&p)
            };
            assert!(RunFilter::decode(&trailing).is_none());
            let sound = envelope(&payload(&[u64::MAX; 2], 128, 7));
            let filter = RunFilter {
                filter: RunFilter::decode(&sound).expect("a sound filter decodes"),
            };
            assert!(filter.might_contain(b"anything"), "every bit is set");
        }

        /// The shape walk reads the golden sidecar the way postcard does.
        #[test]
        fn test_shape_of_the_golden_sidecar() {
            let shape = Shape::read(&GOLDEN[12..]).unwrap();
            assert!(shape.is_sound());
            let filter = golden_filter();
            assert_eq!(shape.num_bits, filter.filter.num_bits());
            assert_eq!(shape.num_hashes, filter.filter.num_hashes());
        }

        #[test]
        fn test_varint_rejects_overlong_and_truncated() {
            let mut pos = 0;
            assert_eq!(varint(&[0x80; 11], &mut pos), None);
            let mut pos = 0;
            assert_eq!(varint(&[0x80], &mut pos), None);
            let mut out = Vec::new();
            leb(u64::MAX, &mut out);
            let mut pos = 0;
            assert_eq!(varint(&out, &mut pos), Some(u64::MAX));
            let mut pos = 0;
            let too_big = [0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x02];
            assert_eq!(varint(&too_big, &mut pos), None);
        }

        #[test]
        fn test_builder_gives_no_filter_when_memory_cannot_be_had() {
            assert!(!can_allocate(usize::MAX));
            assert!(builder(usize::MAX).finish().is_none());
            assert!(can_allocate(1000));
        }

        /// The keys of the golden sidecar.
        fn golden_keys() -> Vec<Vec<u8>> {
            (0..1000u32)
                .map(|i| format!("key-{i:04}").into_bytes())
                .collect()
        }

        fn golden_filter() -> RunFilter {
            let keys = golden_keys();
            let mut b = builder(keys.len());
            for k in &keys {
                b.add(k);
            }
            b.finish().unwrap()
        }

        const GOLDEN: &[u8] = include_bytes!("../tests/data/bloom-sidecar-v1.bin");

        /// Rewrite the golden file from this build. Run only by hand, to
        /// create it: `LSM_WRITE_GOLDEN=1 cargo test --features bloom golden`.
        #[test]
        fn test_write_golden_when_asked() {
            if std::env::var_os("LSM_WRITE_GOLDEN").is_some() {
                let bytes = golden_filter().encode().unwrap();
                std::fs::write("tests/data/bloom-sidecar-v1.bin", bytes).unwrap();
            }
        }

        /// A sidecar written by 1.2.0 decodes, holds every key it was built
        /// from, and encodes to the same bytes; building the same keys today
        /// writes the same bytes. A bloom-lib, postcard, or serde update that
        /// changes the layout fails here instead of silently changing what
        /// runs on disk hold.
        #[test]
        fn test_golden_sidecar_decodes_and_encodes_identically() {
            let filter = RunFilter::decode(GOLDEN).expect("the golden sidecar decodes");
            let filter = RunFilter { filter };
            for key in golden_keys() {
                assert!(filter.might_contain(&key));
            }
            assert_eq!(filter.encode().unwrap(), GOLDEN, "re-encoded bytes differ");
            assert_eq!(
                golden_filter().encode().unwrap(),
                GOLDEN,
                "a fresh build's bytes differ"
            );
        }

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
