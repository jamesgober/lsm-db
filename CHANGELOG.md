# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

---

## [Unreleased]

### Added

### Changed

### Fixed

### Security

---

## [0.8.0] - 2026-06-08

Alpha. The engine is feature-complete, hardened, and API-frozen; this release
begins the soak toward 1.0 by broadening coverage to a sustained,
consumer-shaped workload across restarts. No behaviour or API change — only
additional tests.

### Added

- `tests/soak.rs`: a sustained mixed workload (tens of thousands of interleaved
  puts, overwrites, and deletes over a bounded key space, with a small buffer and
  low compaction trigger so flushes and background compactions run throughout),
  punctuated by close-and-reopen cycles, checked key-for-key and over a full scan
  against a `BTreeMap` reference model. Under `--all-features` it exercises the
  write-ahead log and bloom filters together; a companion test pins ranged scans
  to the model under churn.

---

## [0.7.0] - 2026-06-08

Hardening and the **API freeze**. The engine is run through adversarial,
hostile-input property tests and edge cases, a fuzz harness is added, and the
public API is frozen — no breaking change until 2.0. The on-disk format (frozen
since 0.3) is unchanged.

### Security

- **Fixed a panic on a corrupt bloom sidecar.** A sidecar containing arbitrary
  bytes could `postcard`-deserialize into an internally-inconsistent `bloom-lib`
  filter that panicked (out-of-bounds) when queried. The sidecar is now wrapped
  in a magic + CRC32C integrity envelope, so only bytes this crate actually wrote
  — which always encode a self-consistent filter — are ever deserialized. A
  corrupt or hostile sidecar is discarded and the run is consulted directly.
  (Found by the new adversarial tests.)

### Added

- `tests/adversarial.rs`: property tests that apply arbitrary corruption
  (bit-flips, truncation, garbage) to the run file, manifest, write-ahead log,
  and bloom sidecar, then reopen — asserting the engine returns a `Result` and
  never panics or over-allocates on hostile input.
- `tests/edge_cases.rs`: multi-megabyte values, 50 un-compacted runs, empty and
  64 KiB keys, empty values, and an I/O failure mid-flush surfacing as an `Error`
  rather than a panic.
- `fuzz/`: an isolated `cargo-fuzz` harness (`recover` and `sidecar` targets)
  over the public `Lsm::open` parse/recovery path. Run with
  `cargo +nightly fuzz run <target>`; not built by the normal CI.

### Changed

- **The public API is frozen as of 0.7.0** (until 2.0). The frozen surface is
  recorded in `dev/ROADMAP.md`. Remaining 0.x releases make only additive,
  non-breaking changes.

---

## [0.6.0] - 2026-06-08

Optimization. A block cache serves hot run blocks so repeat point reads do no
I/O, and a comparative benchmark against `sled` and `redb` is documented
honestly. The public API gains one additive config knob; nothing changes
behaviourally.

### Added

- **Block cache** (on by default, 8 MiB): a shared, sharded cache of decoded run
  blocks. A repeat point lookup over a hot working set returns its block from
  cache with no positioned read, no CRC32C check, and no parse. Sequential scans
  and compaction bypass it so they do not pollute it. Eviction is sharded CLOCK
  (the classic O(1) buffer-pool policy); no new runtime dependency.
- `LsmConfig::block_cache_capacity` / `block_cache_capacity_bytes`, and the
  `DEFAULT_BLOCK_CACHE_CAPACITY` constant (8 MiB). Set the capacity to `0` to
  disable the cache.
- `docs/PERFORMANCE.md`: reproducible micro-benchmark numbers, including a
  fair-shape comparison against `sled` 0.34 and `redb` 2.6 (lsm-db leads point
  reads and bulk inserts; redb's range scan is faster — see below).
- A `benches/comparison.rs` benchmark (dev-only) driving all three engines, and
  CI-enforced tests that a cached repeat lookup reads zero data blocks while a
  lookup with the cache disabled reads one.

### Notes

- **Range scan still materialises a snapshot.** The comparison shows `redb`'s
  in-place B-tree scan is faster than lsm-db's, which collects a consistent
  snapshot of the range before returning. Lazy, streaming scan would close the
  gap but requires a *fallible* `Scan` iterator (block I/O moves into iteration),
  which conflicts with the simplified-API mandate and the upcoming 0.7 API
  freeze. It is recorded as a post-1.0 (2.0) consideration in `dev/ROADMAP.md`.
- The cache only affects the point-read path (positively, for hot sets); the
  write and scan paths are unchanged, so there is no regression on them.

---

## [0.5.0] - 2026-06-07

Bloom filters and the feature freeze. Under the `bloom` feature, each sorted run
carries a bloom filter over its keys, so a point read can skip any run that
cannot contain the key — a negative lookup across many runs now reads no data
blocks at all. The engine is feature-complete; the remaining 0.x releases are
optimization (0.6) and hardening with the API frozen (0.7).

The public API is unchanged. The on-disk run format (frozen since 0.3) is
untouched: the filter lives in a sidecar file beside each run.

### Added

- `bloom` feature: a per-run bloom filter (`bloom-lib`) lets a point lookup skip
  any run whose filter rejects the key, with no false negatives. In a benchmark
  of negative lookups across 16 runs this cut a lookup from ~280 µs to ~3 µs.
- Bloom sidecar files (`<run>.sst.bloom`, encoded with `postcard`): the filter
  is written when a run is created and loaded when it is reopened, so the frozen
  run format is not touched. A sidecar is a pure acceleration hint — if it is
  missing or corrupt the run is consulted directly, with identical results.
  Sidecars are removed alongside the runs they describe during compaction.
- Tests: a deterministic, CI-enforced check that a negative lookup reads zero
  data blocks under the `bloom` feature; sidecar round-trip, missing-sidecar and
  corrupt-sidecar graceful-degradation, and sidecar/compaction lifecycle tests;
  a negative-lookup benchmark.
- Examples: `durable_store` (crash-safe writes via `durability`) and
  `bloom_point_reads` (bloom-skipped negative lookups).

### Changed

- `bloom-lib` is pulled (with `serde`) by the `bloom` feature, and `postcard` as
  its sidecar codec.

### Notes

- **Feature freeze declared.** With bloom filters in place the engine is
  feature-complete; only optimization and hardening remain before 1.0.
- **Pluggable comparator dropped from 1.0 scope.** It would require threading a
  generic comparator parameter through every public type (`Lsm<C>`, …), which
  conflicts with the simplified-API mandate; lexicographic byte ordering covers
  the common case (encode keys to sort), matching `sled` and `redb`. Recorded in
  `dev/ROADMAP.md`.

---

## [0.4.0] - 2026-06-07

Durability and crash recovery. Under the `durability` feature, every write is
appended to a `wal-db` write-ahead log and made durable before it is
acknowledged, and the log is replayed on open — so no acknowledged write is lost
across a crash, even one before the next flush. The feature is additive: with it
off, the engine behaves exactly as in 0.3 (durable on flush, fast for caches and
tests).

The public API is unchanged.

### Added

- `durability` feature: a `wal-db`-backed write-ahead log on the write path.
  Each `put` / `delete` / `write` is logged and `fsync`ed before it is applied
  and acknowledged; a batch is logged as one atomic record.
- Crash recovery: on open with the feature enabled, the log is replayed into the
  memtable and checkpointed to a run, so recovery only ever replays the writes
  since the most recent flush. The log is emptied (rotated) after each flush.
- Crash-recovery integration tests (under `--all-features`): un-flushed writes,
  overwrites, deletes, and batches all survive a drop-without-flush and reopen;
  recovery composes with mid-flush, orphan, and corruption handling from 0.3.

### Changed

- `wal-db` is now pulled (with default features off) by the `durability` feature.

### Notes

- `pack-io` record framing (the `framing` feature) is **deferred** — the
  transient, reset-every-flush log gains little from schema-evolution framing,
  and a feature-selected second on-disk codec is not cleanly covered by the
  default / `--all-features` CI matrix. The durable path ships with a single,
  fully-tested record codec. The `framing` flag remains declared as planned.
- Durable writes are serial: each holds the engine write lock across its `fsync`,
  so group commit gives no benefit yet. Batched group commit is an optimisation
  for a later release.

---

## [0.3.0] - 2026-06-06

The real engine: multiple on-disk runs, background compaction, and a frozen
on-disk format. Flushes now append a new sorted run rather than rewriting a
single one; a background thread merges runs into one when they accumulate, so
read amplification stays bounded; and reads merge across the memtable and every
run, newest first. A manifest records the live run set, so a crash mid-flush or
mid-compaction recovers to a consistent state.

The on-disk sorted-run format is **frozen for the 1.x series** and specified
byte-for-byte in `docs/SSTABLE_FORMAT.md`.

The public API is unchanged from 0.2 except for additive configuration.

### Added

- Multi-run storage: each flush writes a new immutable sorted run; reads and
  scans merge across the memtable and all runs with a newest-wins, tombstone-
  aware k-way merge.
- Background compaction: a dedicated thread merges the runs into one when their
  count reaches the configured trigger, concurrent with reads and writes, and
  reclaims superseded run files once no reader still holds them.
- `LsmConfig::compaction_trigger` / `compaction_trigger_runs`, and the
  `DEFAULT_COMPACTION_TRIGGER` constant (4 runs).
- Frozen, block-structured on-disk run format (v1): data blocks with a block
  index, per-block and per-index CRC32C integrity, and tombstones on disk.
  Specified in `docs/SSTABLE_FORMAT.md`.
- On-disk `MANIFEST` recording the live runs in recency order plus the next run
  sequence number; rewritten atomically on every flush and compaction.
- Crash recovery on open: the manifest is the source of truth; temporary files
  and run files it does not name are reclaimed as orphans.
- `crc32c` dependency for hardware-accelerated run checksums.
- Tests: property test of compaction against a model; concurrent-writer stress
  test with background compaction; crash-recovery tests (ungraceful exit, stale
  temp file, orphan run, missing run, corrupted block); `loom` model of the
  read-versus-compaction swap protocol.

### Changed

- Flush no longer merges into a single run; it appends a new run and lets
  compaction consolidate. The on-disk layout from 0.2 (which was explicitly not
  frozen) is replaced by the v1 format; 0.2 data directories are not read.

---

## [0.2.0] - 2026-06-06

The foundation release: a working single-run storage engine with the Tier-1 API
locked in. Writes buffer in a sorted in-memory memtable and flush to an
immutable, `fsync`ed sorted run on disk; reads check the buffer and fall through
to the run; deletes are tombstones that resolve on flush. Flushed data survives
reopening.

The on-disk format is **not** frozen yet — it is finalised with the multi-level
engine in 0.3. Durability of un-flushed writes (write-ahead logging) and bloom
filters are also still ahead on the roadmap.

### Added

- `Lsm` — the Tier-1 engine: `open`, `open_with`, `put`, `get`, `delete`,
  `scan`, `write`, and `flush`. Every method takes `&self`; the type is
  `Send + Sync` for sharing behind an `Arc`.
- `LsmConfig` — Tier-2 tuning for the memtable capacity, plus the
  `DEFAULT_MEMTABLE_CAPACITY` constant (4 MiB).
- `Batch` — grouped writes applied atomically with respect to concurrent
  readers via `Lsm::write`.
- `Scan` — an ascending `(key, value)` iterator returned by `Lsm::scan`, taken
  as a consistent point-in-time snapshot of the requested range.
- `Error` and `Result` — the domain error type, integrated with `error-forge`
  (`ForgeError`) and exposing the underlying `io::Error` as its source.
- `prelude` module re-exporting the common surface.
- Sorted in-memory memtable with approximate size accounting and tombstones.
- On-disk sorted-run writer and reader: atomic flush (temp file, `fsync`,
  rename), an in-memory key index, and cross-platform positioned reads
  (`pread` on Unix, `seek_read` on Windows) so concurrent readers share one
  handle. Length prefixes are bounded to reject corrupt or hostile runs.
- Property tests (`proptest`) checking `get`/`scan` against a `BTreeMap` model
  across memtable sizes, plus flush-and-reopen and sub-range coverage.
- Integration tests for multi-flush workloads, reopen, atomic batches, and
  concurrent readers with a writer.
- `criterion` benchmarks for point write, point read (hit and miss), and scan.
- Examples: `embedded_kv`, `range_scan`, `batch_writes`.

### Changed

- Bumped first-party dependency requirements to their published `1.0` releases
  (`error-forge`, `wal-db`, `bloom-lib`, `pack-io`) and added `error-forge` as a
  direct dependency for the error type.

---

## [0.1.0] - 2026-05-30

Initial scaffold and repository bootstrap. No lsm-db logic yet &mdash; this release establishes the structure, tooling, and quality gates the implementation will be built on.

### Added

- `Cargo.toml` with full crate metadata, Rust 2024 edition, MSRV 1.85, dual `Apache-2.0 OR MIT` license, `docs.rs` configuration, perf-tuned release profile.
- Feature flags and first-party dependency wiring (see `Cargo.toml`).
- Dev-dependencies for the test stack: `criterion`, `proptest`, and `loom` under `cfg(loom)`.
- `README.md` &mdash; overview, positioning, install, and "where it fits".
- `docs/API.md` reference skeleton.
- `REPS.md` compliance baseline at the repository root.
- `.github/workflows/ci.yml` &mdash; Linux/macOS/Windows CI matrix on stable and MSRV, plus loom and audit/deny jobs.
- `deny.toml`, `clippy.toml`, `rustfmt.toml`, `.gitattributes`, `.gitignore`.
- `.dev/` AI-editor briefing (`PROMPT.md`, `ROADMAP.md`) &mdash; gitignored.

[Unreleased]: https://github.com/jamesgober/lsm-db/compare/v0.8.0...HEAD
[0.8.0]: https://github.com/jamesgober/lsm-db/compare/v0.7.0...v0.8.0
[0.7.0]: https://github.com/jamesgober/lsm-db/compare/v0.6.0...v0.7.0
[0.6.0]: https://github.com/jamesgober/lsm-db/compare/v0.5.0...v0.6.0
[0.5.0]: https://github.com/jamesgober/lsm-db/compare/v0.4.0...v0.5.0
[0.4.0]: https://github.com/jamesgober/lsm-db/compare/v0.3.0...v0.4.0
[0.3.0]: https://github.com/jamesgober/lsm-db/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/jamesgober/lsm-db/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/jamesgober/lsm-db/releases/tag/v0.1.0
