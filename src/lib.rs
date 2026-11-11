//! # lsm-db
//!
//! A log-structured merge-tree storage engine for Rust.
//!
//! An LSM engine is the write path that powers RocksDB, LevelDB, Cassandra, and
//! ScyllaDB: writes accumulate in a sorted in-memory buffer (the *memtable*);
//! when the buffer fills it is flushed to an immutable, sorted file on disk (an
//! *SSTable*); reads consult the buffer first and fall through to the file. The
//! design turns random writes into sequential disk writes, which is why it
//! underpins so many write-heavy stores.
//!
//! `lsm-db` packages that write path as a small, audited library so the storage
//! engines in the portfolio — `txn-db`, Hive DB — share one implementation
//! rather than each re-deriving it.
//!
//! ## The Tier-1 API
//!
//! The common case is five calls: open, put, get, delete, scan.
//!
//! ```
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! use lsm_db::Lsm;
//!
//! // Open (or create) a database backed by a directory.
//! let dir = tempfile::tempdir()?;
//! let db = Lsm::open(dir.path())?;
//!
//! // Write and read arbitrary byte keys and values.
//! db.put(b"user:1", b"alice")?;
//! db.put(b"user:2", b"bob")?;
//! assert_eq!(db.get(b"user:1")?, Some(b"alice".to_vec()));
//!
//! // Delete masks the key.
//! db.delete(b"user:1")?;
//! assert_eq!(db.get(b"user:1")?, None);
//!
//! // Range scans walk keys in sorted order.
//! db.put(b"user:1", b"alice")?;
//! let users: Vec<_> = db.scan(b"user:".to_vec()..b"user;".to_vec())?.collect();
//! assert_eq!(users.len(), 2);
//! # Ok(())
//! # }
//! ```
//!
//! ## Tuning
//!
//! [`LsmConfig`] is the Tier-2 surface for tuning the write-buffer size. Pass it
//! to [`Lsm::open_with`]; [`Lsm::open`] uses the defaults.
//!
//! ```
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! use lsm_db::{Lsm, LsmConfig};
//! let dir = tempfile::tempdir()?;
//! let db = Lsm::open_with(dir.path(), LsmConfig::new().memtable_capacity(1 << 20))?;
//! db.put(b"k", b"v")?;
//! # Ok(())
//! # }
//! ```
//!
//! ## Grouped writes
//!
//! [`Batch`] applies several writes as one atomic group; see [`Lsm::write`].
//!
//! ## Durability
//!
//! Flushed runs are `fsync`ed and recorded in a manifest, so flushed data
//! survives reopening, and the on-disk format is frozen for the 1.x series.
//! Writes still buffered in the memtable when a process exits without
//! [`flush`](Lsm::flush)ing are durable only with the `durability` feature: it
//! logs every write to a `wal-db` write-ahead log before acknowledging it and
//! replays the log on open, so no acknowledged write is lost across a crash.
//!
//! ## Feature flags
//!
//! | Feature | Default | Description |
//! |---------|---------|-------------|
//! | `std` | yes | Standard library. The engine (`Lsm`, `LsmConfig`, `Batch`, `Scan`) requires it. Implies `alloc`, and adds the `error-forge` integration and run-time CRC32C hardware detection. |
//! | `alloc` | via `std` | Heap allocation without `std`: the [`format`](mod@format) codec (`RunWriter`, `RunReader`). |
//! | `durability` | no | Crash-safe writes via a `wal-db` write-ahead log. Implies `std`. |
//! | `bloom` | no | Bloom-filtered point lookups via `bloom-lib`. Implies `std`. |
//!
//! ## `no_std`
//!
//! With `default-features = false` the crate builds on bare-metal targets. The
//! storage engine needs a filesystem and stays behind `std`; what remains is
//! the on-disk format itself. Without any feature that is [`format::crc32c`],
//! [`Error`], and [`Result`]; with `alloc` it adds [`format::RunWriter`] and
//! [`format::RunReader`], which write and read sorted runs in memory,
//! byte-compatible with the files the engine writes.
//!
//! ```toml
//! [dependencies]
//! lsm-db = { version = "1.3", default-features = false, features = ["alloc"] }
//! ```
//!
//! CRC32C is part of the format, so every build computes identical checksums.
//! With `std` the crate asks the CPU at run time and uses SSE4.2 or the ARMv8
//! CRC extension when present, slicing-by-8 tables otherwise. Without `std`
//! there is no run-time detection: the hardware instructions are used when the
//! target enables them at compile time (`-C target-feature=+sse4.2` on x86_64,
//! `+crc` on aarch64) and the tables otherwise.

#![cfg_attr(not(feature = "std"), no_std)]
#![cfg_attr(docsrs, feature(doc_cfg))]
#![deny(warnings)]
#![deny(missing_docs)]
// No `unsafe` anywhere, except the CRC32C instructions in `src/crc.rs`, module
// `hw`: compiled in `std` builds on x86_64 and aarch64 (used after run-time
// detection) and in `no_std` builds whose target enables them. Every other
// build forbids it outright.
#![cfg_attr(
    not(any(
        all(
            not(feature = "std"),
            any(
                all(target_arch = "x86_64", target_feature = "sse4.2"),
                all(target_arch = "aarch64", target_feature = "crc")
            )
        ),
        all(feature = "std", any(target_arch = "x86_64", target_arch = "aarch64"))
    )),
    forbid(unsafe_code)
)]
#![cfg_attr(
    any(
        all(
            not(feature = "std"),
            any(
                all(target_arch = "x86_64", target_feature = "sse4.2"),
                all(target_arch = "aarch64", target_feature = "crc")
            )
        ),
        all(feature = "std", any(target_arch = "x86_64", target_arch = "aarch64"))
    ),
    deny(unsafe_code)
)]
#![deny(unused_must_use)]
#![deny(unused_results)]
#![deny(clippy::unwrap_used)]
#![deny(clippy::expect_used)]
#![deny(clippy::todo)]
#![deny(clippy::unimplemented)]
#![deny(clippy::print_stdout)]
#![deny(clippy::print_stderr)]
#![deny(clippy::dbg_macro)]
#![deny(clippy::unreachable)]

#[cfg(any(feature = "alloc", test))]
extern crate alloc;
#[cfg(all(test, not(feature = "std")))]
extern crate std;

#[cfg(feature = "std")]
mod batch;
#[cfg(feature = "std")]
mod bloom;
#[cfg(feature = "std")]
mod cache;
#[cfg(feature = "std")]
mod config;
mod crc;
#[cfg(feature = "std")]
mod db;
#[cfg(feature = "std")]
mod durability;
mod error;
pub mod format;
#[cfg(feature = "std")]
mod manifest;
#[cfg(feature = "std")]
mod memtable;
#[cfg(feature = "std")]
mod merge;
#[cfg(feature = "std")]
mod record;
#[cfg(feature = "std")]
mod scan;
#[cfg(feature = "std")]
mod sstable;

#[cfg(feature = "std")]
pub use crate::batch::Batch;
#[cfg(feature = "std")]
pub use crate::config::{
    DEFAULT_BLOCK_CACHE_CAPACITY, DEFAULT_COMPACTION_TRIGGER, DEFAULT_MEMTABLE_CAPACITY, LsmConfig,
};
#[cfg(feature = "std")]
pub use crate::db::Lsm;
pub use crate::error::{Error, Result};
#[cfg(feature = "std")]
pub use crate::scan::Scan;

/// The crate's common imports in one `use`.
///
/// ```
/// use lsm_db::prelude::*;
/// # fn main() -> Result<()> {
/// let dir = tempfile::tempdir().map_err(Error::from)?;
/// let db = Lsm::open(dir.path())?;
/// db.put(b"k", b"v")?;
/// # Ok(())
/// # }
/// ```
#[cfg(feature = "std")]
pub mod prelude {
    pub use crate::{Batch, Error, Lsm, LsmConfig, Result, Scan};
}
