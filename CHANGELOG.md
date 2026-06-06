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

[Unreleased]: https://github.com/jamesgober/lsm-db/compare/v0.2.0...HEAD
[0.2.0]: https://github.com/jamesgober/lsm-db/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/jamesgober/lsm-db/releases/tag/v0.1.0
