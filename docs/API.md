<h1 align="center">
    <img width="99" alt="Rust logo" src="https://raw.githubusercontent.com/jamesgober/rust-collection/72baabd71f00e14aa9184efcb16fa3deddda3a0a/assets/rust-logo.svg">
    <br><b>lsm-db</b><br>
    <sub><sup>API REFERENCE</sup></sub>
</h1>
<div align="center">
    <sup>
        <a href="../README.md" title="Project Home"><b>HOME</b></a>
        <span>&nbsp;│&nbsp;</span>
        <span>API</span>
        <span>&nbsp;│&nbsp;</span>
        <a href="../CHANGELOG.md" title="Changelog"><b>CHANGELOG</b></a>
    </sup>
</div>
<br>

> Complete reference for every public item in `lsm-db`, with parameter notes and
> runnable examples.
>
> **Status: `1.3.0`, stable. API frozen until 2.0.** The surface below is frozen
> — no breaking change — over a multi-run engine with background compaction, a
> block cache, optional crash-safe writes (`durability`), and optional
> bloom-filtered point reads (`bloom`). The on-disk format is frozen for the 1.x
> series ([`docs/SSTABLE_FORMAT.md`](./SSTABLE_FORMAT.md)). Since 1.1 the format
> is also available as an in-memory codec, [`format`](#format), which builds
> without `std`.

<h4 id="example-pointers">Example Pointers</h4>

- Embedded KV: `examples/embedded_kv.rs` — open, put, get, overwrite, delete, flush.
- Range scan: `examples/range_scan.rs` — full, bounded, and prefix scans in key order.
- Batch writes: `examples/batch_writes.rs` — grouped atomic writes and reopen.
- Run codec: `examples/run_codec.rs` — read a flushed run file and build a run in memory with `format`.

<br>

## Table of Contents

- [Installation](#installation)
- [Overview](#overview)
- [Quick Start](#quick-start)
- [The three tiers](#the-three-tiers)
- [Public APIs](#public-apis)
  - [`Lsm`](#lsm)
    - [`Lsm::open`](#lsmopen)
    - [`Lsm::open_with`](#lsmopen_with)
    - [`Lsm::put`](#lsmput)
    - [`Lsm::get`](#lsmget)
    - [`Lsm::delete`](#lsmdelete)
    - [`Lsm::write`](#lsmwrite)
    - [`Lsm::scan`](#lsmscan)
    - [`Lsm::scan_prefix`](#lsmscan_prefix)
    - [`Lsm::flush`](#lsmflush)
    - [`Lsm::close`](#lsmclose)
  - [`LsmConfig`](#lsmconfig)
  - [`DEFAULT_MEMTABLE_CAPACITY`](#default_memtable_capacity)
  - [`DEFAULT_COMPACTION_TRIGGER`](#default_compaction_trigger)
  - [`DEFAULT_BLOCK_CACHE_CAPACITY`](#default_block_cache_capacity)
  - [`Batch`](#batch)
  - [`Scan`](#scan)
  - [`Error` & `Result`](#error--result)
  - [`prelude`](#prelude)
  - [`format`](#format)
    - [`RunWriter`](#runwriter)
    - [`RunReader`](#runreader)
    - [`Entry`](#entry)
    - [`crc32c`](#crc32c)
    - [`MAX_KEY_LEN` & `MAX_VALUE_LEN`](#max_key_len--max_value_len)
- [Concurrency](#concurrency)
- [Durability & persistence](#durability--persistence)
- [Feature flags](#feature-flags)
- [`no_std`](#no_std)

---

## Installation

```toml
[dependencies]
lsm-db = "1.3"
```

The engine requires the standard library, which is on by default. See
[Feature flags](#feature-flags) for the optional first-party integrations, and
[`no_std`](#no_std) for bare-metal targets.

---

## Overview

`lsm-db` is a log-structured merge-tree storage engine. Writes accumulate in a
sorted in-memory buffer (the *memtable*); when the buffer reaches its configured
capacity it is flushed to an immutable, sorted file on disk (a *sorted run*, or
SSTable); reads consult the buffer first and fall through to the run. Keys and
values are arbitrary byte strings, and keys are ordered lexicographically.

The common case is five calls — `open`, `put`, `get`, `delete`, `scan` — over
the [`Lsm`](#lsm) type.

---

## Quick Start

```rust
use lsm_db::Lsm;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let dir = tempfile::tempdir()?;
    let db = Lsm::open(dir.path())?;

    db.put(b"hello", b"world")?;
    assert_eq!(db.get(b"hello")?, Some(b"world".to_vec()));

    db.delete(b"hello")?;
    assert_eq!(db.get(b"hello")?, None);
    Ok(())
}
```

---

## The three tiers

`lsm-db` follows the portfolio's tiered-API convention:

- **Tier 1 — the common case.** [`Lsm::open`](#lsmopen) plus
  [`put`](#lsmput) / [`get`](#lsmget) / [`delete`](#lsmdelete) /
  [`scan`](#lsmscan). No builder, no generics to name.
- **Tier 2 — tuning.** [`LsmConfig`](#lsmconfig) passed to
  [`Lsm::open_with`](#lsmopen_with), and [`Batch`](#batch) for grouped writes.

There is no Tier-3 trait seam in the 1.0 surface: keys are ordered
lexicographically and the engine is concrete. A pluggable comparator was
considered and deliberately left out to keep the API simple (encode keys to sort
when you need a custom order, as with `sled` / `redb`).

---

## Public APIs

### `Lsm`

```rust
pub struct Lsm { /* ... */ }
```

The storage engine: a key-value store backed by a directory on disk. Construct
it with [`open`](#lsmopen) or [`open_with`](#lsmopen_with). Every method takes
`&self`, so a single engine can be shared — see [Concurrency](#concurrency).

`Lsm` is `Send + Sync` and `Debug`.

---

#### `Lsm::open`

```rust
pub fn open(dir: impl AsRef<Path>) -> Result<Lsm>
```

Open the database in `dir`, creating the directory if it does not exist, using
the [default configuration](#lsmconfig). Any sorted run left by a previous
session is reopened, so flushed data is visible immediately. A leftover
temporary file from a flush interrupted by a crash is discarded — the previous
run remains authoritative.

**Parameters**

- `dir` — the database directory. Anything that is `AsRef<Path>` works: a
  `&str`, `String`, `Path`, or `PathBuf`.

**Returns** an [`Lsm`], or an [`Error::Io`](#error--result) if the directory
cannot be created, or [`Error::Corruption`](#error--result) if an existing run
is damaged.

```rust
# fn main() -> Result<(), Box<dyn std::error::Error>> {
use lsm_db::Lsm;
let dir = tempfile::tempdir()?;

// Open by path.
let db = Lsm::open(dir.path())?;
db.put(b"k", b"v")?;
drop(db); // dropping flushes the buffer (since 1.2.1)

// Reopen the same directory; the write is there.
let db = Lsm::open(dir.path())?;
assert_eq!(db.get(b"k")?, Some(b"v".to_vec()));
db.flush()?; // nothing buffered, no-op
# Ok(())
# }
```

---

#### `Lsm::open_with`

```rust
pub fn open_with(dir: impl AsRef<Path>, config: LsmConfig) -> Result<Lsm>
```

Open the database in `dir` with an explicit [`LsmConfig`](#lsmconfig). Identical
to [`open`](#lsmopen) except that it takes a configuration instead of using the
default.

**Parameters**

- `dir` — the database directory (`AsRef<Path>`).
- `config` — the tuning parameters; see [`LsmConfig`](#lsmconfig).

```rust
# fn main() -> Result<(), Box<dyn std::error::Error>> {
use lsm_db::{Lsm, LsmConfig};
let dir = tempfile::tempdir()?;

// Flush after every 64 KiB of buffered key/value data.
let config = LsmConfig::new().memtable_capacity(64 * 1024);
let db = Lsm::open_with(dir.path(), config)?;
db.put(b"k", b"v")?;
# Ok(())
# }
```

---

#### `Lsm::put`

```rust
pub fn put(&self, key: impl AsRef<[u8]>, value: impl AsRef<[u8]>) -> Result<()>
```

Set `key` to `value`, overwriting any previous value. The write lands in the
in-memory buffer and triggers a flush if the buffer has reached its configured
capacity.

**Parameters**

- `key` — the key bytes (`AsRef<[u8]>`: `&[u8]`, `Vec<u8>`, `&str`, …). Copied
  into the engine, so the caller's buffer is free to reuse.
- `value` — the value bytes (`AsRef<[u8]>`). Empty values are allowed.

**Errors** `Error::InvalidInput` if the key or value is longer than
[`MAX_KEY_LEN` / `MAX_VALUE_LEN`](#max_key_len--max_value_len) (1 GiB each);
nothing is written. `Error::Io` if logging the write or a resulting flush fails.

```rust
# fn main() -> Result<(), Box<dyn std::error::Error>> {
# let dir = tempfile::tempdir()?;
# let db = lsm_db::Lsm::open(dir.path())?;
db.put(b"byte-key", b"byte-value")?;
db.put("string-key", "string-value")?;     // &str works too
db.put(vec![1u8, 2, 3], vec![4u8, 5, 6])?; // owned Vec works too
db.put(b"empty", b"")?;                     // empty value
assert_eq!(db.get(b"empty")?, Some(Vec::new()));
# Ok(())
# }
```

---

#### `Lsm::get`

```rust
pub fn get(&self, key: impl AsRef<[u8]>) -> Result<Option<Vec<u8>>>
```

Look up `key`, returning its value, or `None` if it is absent or deleted. The
buffer is checked first, then the on-disk run.

**Parameters**

- `key` — the key bytes (`AsRef<[u8]>`).

**Returns** `Some(value)` if the key is live, `None` if absent or tombstoned, or
an [`Error`](#error--result) on an I/O failure or a corrupt run.

```rust
# fn main() -> Result<(), Box<dyn std::error::Error>> {
# let dir = tempfile::tempdir()?;
# let db = lsm_db::Lsm::open(dir.path())?;
assert_eq!(db.get(b"missing")?, None);
db.put(b"present", b"1")?;
assert_eq!(db.get(b"present")?, Some(b"1".to_vec()));
# Ok(())
# }
```

---

#### `Lsm::delete`

```rust
pub fn delete(&self, key: impl AsRef<[u8]>) -> Result<()>
```

Delete `key`; a subsequent [`get`](#lsmget) returns `None`. Deleting a key that
is not present is not an error. Internally a delete records a tombstone that
masks any older on-disk value until a flush resolves it away.

**Parameters**

- `key` — the key bytes (`AsRef<[u8]>`).

**Errors** `Error::InvalidInput` if the key is longer than
[`MAX_KEY_LEN`](#max_key_len--max_value_len); nothing is written.

```rust
# fn main() -> Result<(), Box<dyn std::error::Error>> {
# let dir = tempfile::tempdir()?;
# let db = lsm_db::Lsm::open(dir.path())?;
db.put(b"k", b"v")?;
db.delete(b"k")?;
assert_eq!(db.get(b"k")?, None);

db.delete(b"never-existed")?; // not an error

// Delete then re-put revives the key.
db.put(b"k", b"again")?;
assert_eq!(db.get(b"k")?, Some(b"again".to_vec()));
# Ok(())
# }
```

---

#### `Lsm::write`

```rust
pub fn write(&self, batch: Batch) -> Result<()>
```

Apply a [`Batch`](#batch) of writes as one group. The whole batch is applied
under a single lock acquisition, so concurrent readers observe either none or
all of it. Operations within the batch take effect in call order, so a later
operation on a key overrides an earlier one.

**Parameters**

- `batch` — the [`Batch`](#batch) to apply; consumed by the call.

**Errors** `Error::InvalidInput` if any key or value in the batch is over
[the limit](#max_key_len--max_value_len); none of the batch is written.

```rust
# fn main() -> Result<(), Box<dyn std::error::Error>> {
use lsm_db::Batch;
# let dir = tempfile::tempdir()?;
# let db = lsm_db::Lsm::open(dir.path())?;
let mut batch = Batch::new();
batch.put(b"a", b"1");
batch.put(b"b", b"2");
batch.delete(b"c");
db.write(batch)?;

assert_eq!(db.get(b"a")?, Some(b"1".to_vec()));
assert_eq!(db.get(b"b")?, Some(b"2".to_vec()));
# Ok(())
# }
```

---

#### `Lsm::scan`

```rust
pub fn scan<R>(&self, range: R) -> Result<Scan>
where
    R: RangeBounds<Vec<u8>>,
```

Iterate the live `(key, value)` pairs whose key falls in `range`, in ascending
key order. Deleted keys are already resolved away. The returned
[`Scan`](#scan) is a consistent snapshot taken when `scan` is called; later
writes do not affect it.

**Parameters**

- `range` — any range over `Vec<u8>` bounds. All the usual syntaxes work:
  `..` (everything), `a..b` (half-open), `a..=b` (inclusive), `a..`, `..b`.

```rust
# fn main() -> Result<(), Box<dyn std::error::Error>> {
# let dir = tempfile::tempdir()?;
# let db = lsm_db::Lsm::open(dir.path())?;
db.put(b"a", b"1")?;
db.put(b"b", b"2")?;
db.put(b"c", b"3")?;

// Everything.
assert_eq!(db.scan(..)?.count(), 3);

// Half-open range [a, c).
let half: Vec<_> = db.scan(b"a".to_vec()..b"c".to_vec())?.collect();
assert_eq!(half, vec![(b"a".to_vec(), b"1".to_vec()), (b"b".to_vec(), b"2".to_vec())]);

// Inclusive range [a, b].
let incl: Vec<_> = db.scan(b"a".to_vec()..=b"b".to_vec())?.collect();
assert_eq!(incl.len(), 2);

// Prefix scan: everything under "b".
let prefix: Vec<_> = db.scan(b"b".to_vec()..b"c".to_vec())?.collect();
assert_eq!(prefix, vec![(b"b".to_vec(), b"2".to_vec())]);
# Ok(())
# }
```

---

#### `Lsm::flush`

```rust
pub fn flush(&self) -> Result<()>
```

Force the in-memory buffer to disk, merging it into the sorted run. Flushing an
empty buffer is a no-op. After a successful flush every previously written key
is durable and will be read back on reopen.

```rust
# fn main() -> Result<(), Box<dyn std::error::Error>> {
let dir = tempfile::tempdir()?;
{
    let db = lsm_db::Lsm::open(dir.path())?;
    db.put(b"k", b"v")?;
    db.flush()?;
}
// A fresh process opens the same directory and sees the flushed data.
let db = lsm_db::Lsm::open(dir.path())?;
assert_eq!(db.get(b"k")?, Some(b"v".to_vec()));
# Ok(())
# }
```

#### `Lsm::close`

```rust
pub fn close(self) -> Result<()>
```

Since 1.3. Stop the background compactor and flush the buffer, returning the
flush error. Dropping the handle does the same but cannot report an error, so
call `close` where the caller can act on one (a full disk, a failed sync).

```rust
# fn main() -> Result<(), Box<dyn std::error::Error>> {
let dir = tempfile::tempdir()?;
let db = lsm_db::Lsm::open(dir.path())?;
db.put(b"k", b"v")?;
db.close()?;
let db = lsm_db::Lsm::open(dir.path())?;
assert_eq!(db.get(b"k")?, Some(b"v".to_vec()));
# Ok(())
# }
```

#### `Lsm::scan_prefix`

```rust
pub fn scan_prefix(&self, prefix: &[u8]) -> Result<Scan>
```

Since 1.3. Every key that starts with `prefix`, with its value, in ascending
key order, as one snapshot like [`scan`](#lsmscan). A prefix made only of
`0xFF` bytes, or an empty one, has no key after all its keys, so its scan runs
to the end of the key space. Like every scan since 1.3, it seeks to its start,
so its cost follows the keys it returns.

```rust
# fn main() -> Result<(), Box<dyn std::error::Error>> {
let dir = tempfile::tempdir()?;
let db = lsm_db::Lsm::open(dir.path())?;
db.put(b"user:1", b"ann")?;
db.put(b"user:2", b"bo")?;
db.put(b"users", b"not a user key")?;
let users: Vec<_> = db.scan_prefix(b"user:")?.map(|(k, _)| k).collect();
assert_eq!(users, vec![b"user:1".to_vec(), b"user:2".to_vec()]);
# Ok(())
# }
```

---

### `LsmConfig`

```rust
pub struct LsmConfig { /* ... */ }
```

Tier-2 tuning parameters, passed to [`Lsm::open_with`](#lsmopen_with). Build with
[`new`](#lsmconfig) (or [`default`]) and refine with chained setters.

| Method | Description |
|--------|-------------|
| `LsmConfig::new() -> LsmConfig` | Start from the default configuration. |
| `LsmConfig::default() -> LsmConfig` | Same as `new`; default buffer and compaction trigger. |
| `.memtable_capacity(bytes: usize) -> LsmConfig` | Set the write-buffer size, in bytes of live key + value data. Consumes and returns `self`. |
| `.memtable_capacity_bytes(&self) -> usize` | Read the configured capacity. |
| `.compaction_trigger(runs: usize) -> LsmConfig` | Set the run count that triggers a background compaction. Values below `2` become `2`. Consumes and returns `self`. |
| `.compaction_trigger_runs(&self) -> usize` | Read the configured trigger. |
| `.block_cache_capacity(bytes: usize) -> LsmConfig` | Set the block-cache capacity, in bytes of decoded blocks. `0` disables the cache. Consumes and returns `self`. |
| `.block_cache_capacity_bytes(&self) -> usize` | Read the configured block-cache capacity. |

The capacity counts key and value bytes only, not per-entry bookkeeping, so peak
resident memory is somewhat higher than the configured number. A capacity of `0`
flushes after every write — useful in tests, rarely otherwise.

The compaction trigger bounds read amplification: each flush adds a run, and a
point read may consult every run, so the engine merges the runs into one in the
background once there are this many. Smaller values keep reads fast at the cost
of more compaction work.

The block cache (default 8 MiB) keeps recently-read decoded run blocks so a
repeat point lookup over a hot working set returns with no I/O, checksum, or
parse. It is shared across all of an engine's runs; set the capacity to `0` to
disable it.

```rust
use lsm_db::LsmConfig;

// 1 MiB write buffer; compact once eight runs pile up; 32 MiB block cache.
let config = LsmConfig::new()
    .memtable_capacity(1 << 20)
    .compaction_trigger(8)
    .block_cache_capacity(32 << 20);
assert_eq!(config.memtable_capacity_bytes(), 1 << 20);
assert_eq!(config.compaction_trigger_runs(), 8);
assert_eq!(config.block_cache_capacity_bytes(), 32 << 20);

// The defaults.
assert_eq!(
    LsmConfig::default().memtable_capacity_bytes(),
    lsm_db::DEFAULT_MEMTABLE_CAPACITY,
);
assert_eq!(
    LsmConfig::default().compaction_trigger_runs(),
    lsm_db::DEFAULT_COMPACTION_TRIGGER,
);
```

---

### `DEFAULT_MEMTABLE_CAPACITY`

```rust
pub const DEFAULT_MEMTABLE_CAPACITY: usize = 4 * 1024 * 1024; // 4 MiB
```

The memtable capacity used by [`LsmConfig::default`] and [`Lsm::open`](#lsmopen).

```rust
assert_eq!(lsm_db::DEFAULT_MEMTABLE_CAPACITY, 4 * 1024 * 1024);
```

---

### `DEFAULT_COMPACTION_TRIGGER`

```rust
pub const DEFAULT_COMPACTION_TRIGGER: usize = 4; // runs
```

The run count that triggers a background compaction by default.

```rust
assert_eq!(lsm_db::DEFAULT_COMPACTION_TRIGGER, 4);
```

---

### `DEFAULT_BLOCK_CACHE_CAPACITY`

```rust
pub const DEFAULT_BLOCK_CACHE_CAPACITY: usize = 8 * 1024 * 1024; // 8 MiB
```

The block-cache capacity used by [`LsmConfig::default`].

```rust
assert_eq!(lsm_db::DEFAULT_BLOCK_CACHE_CAPACITY, 8 * 1024 * 1024);
```

---

### `Batch`

```rust
pub struct Batch { /* ... */ }
```

An ordered group of writes applied together by [`Lsm::write`](#lsmwrite).
Operations are replayed in call order, so a later operation on a key overrides
an earlier one.

| Method | Description |
|--------|-------------|
| `Batch::new() -> Batch` | Create an empty batch. |
| `.put(key: impl AsRef<[u8]>, value: impl AsRef<[u8]>)` | Queue a put. Both are copied in. |
| `.delete(key: impl AsRef<[u8]>)` | Queue a delete. |
| `.len(&self) -> usize` | Number of queued operations. |
| `.is_empty(&self) -> bool` | Whether the batch has no operations. |

`Batch` is `Clone`, `Debug`, and `Default`.

```rust
use lsm_db::Batch;

let mut batch = Batch::new();
batch.put(b"alpha", b"1");
batch.put(b"beta", b"2");
batch.delete(b"gamma");
assert_eq!(batch.len(), 3);
assert!(!batch.is_empty());
```

```rust
# fn main() -> Result<(), Box<dyn std::error::Error>> {
use lsm_db::{Batch, Lsm};
# let dir = tempfile::tempdir()?;
let db = Lsm::open(dir.path())?;

// Load many keys in one grouped, atomic write.
let mut batch = Batch::new();
for i in 0..1_000u32 {
    batch.put(format!("k{i:04}").into_bytes(), b"v");
}
db.write(batch)?;
assert_eq!(db.scan(..)?.count(), 1_000);
# Ok(())
# }
```

---

### `Scan`

```rust
pub struct Scan { /* ... */ }
```

The ascending iterator returned by [`Lsm::scan`](#lsmscan). It yields
`(Vec<u8>, Vec<u8>)` `(key, value)` pairs in ascending key order and implements
[`Iterator`], [`ExactSizeIterator`], and [`DoubleEndedIterator`].

```rust
# fn main() -> Result<(), Box<dyn std::error::Error>> {
# let dir = tempfile::tempdir()?;
# let db = lsm_db::Lsm::open(dir.path())?;
db.put(b"a", b"1")?;
db.put(b"b", b"2")?;
db.put(b"c", b"3")?;

let scan = db.scan(..)?;
assert_eq!(scan.len(), 3);                  // ExactSizeIterator

// Iterate forward.
let forward: Vec<_> = db.scan(..)?.map(|(k, _)| k).collect();
assert_eq!(forward, vec![b"a".to_vec(), b"b".to_vec(), b"c".to_vec()]);

// Iterate in reverse (DoubleEndedIterator).
let reverse: Vec<_> = db.scan(..)?.rev().map(|(k, _)| k).collect();
assert_eq!(reverse, vec![b"c".to_vec(), b"b".to_vec(), b"a".to_vec()]);
# Ok(())
# }
```

---

### `Error` & `Result`

```rust
pub type Result<T, E = Error> = core::result::Result<T, E>;

#[non_exhaustive]
pub enum Error {
    #[cfg(feature = "std")]
    Io { context: &'static str, source: std::io::Error },
    Corruption { reason: &'static str },
    InvalidInput { reason: &'static str },
}
```

The domain error type for every fallible operation. It is `#[non_exhaustive]`,
so a `match` over it must include a wildcard arm.

| Variant | Meaning | Caller action |
|---------|---------|---------------|
| `Io` | An underlying I/O operation failed. `context` names what was attempted; the original `io::Error` is the [`source`](https://doc.rust-lang.org/std/error/trait.Error.html#method.source). Only with `std`. | Inspect the OS error kind (disk full, permission denied) via the source. May be retryable. |
| `Corruption` | An on-disk run is not intact (bad magic, implausible length, truncation, checksum mismatch). | Not retryable; the bytes on disk are damaged. |
| `InvalidInput` | The caller passed something the format cannot store: a key or value over 1 GiB, or keys out of order in a [`RunWriter`](#runwriter). Nothing was written. *(since 1.1)* | Fix the input and retry. |

`Error` implements `core::error::Error` (the same trait as `std::error::Error`)
and `Display`. With `std` it also implements
[`error_forge::ForgeError`](https://docs.rs/error-forge) — `kind()` returns
`"Io"` / `"Corruption"` / `"InvalidInput"`, `caption()` returns
`"lsm storage engine error"`, and `is_fatal()` is `true` only for `Corruption`
— and a bare `std::io::Error` converts into `Error::Io` via `From`, for `?`
ergonomics.

```rust
use lsm_db::Error;
use error_forge::ForgeError;

# fn main() -> Result<(), Box<dyn std::error::Error>> {
let dir = tempfile::tempdir().map_err(Error::from)?;
let db = lsm_db::Lsm::open(dir.path())?;
db.put(b"k", b"v")?;

// Errors carry actionable metadata.
fn classify(err: &Error) -> bool {
    err.is_fatal() // true only for corruption
}
# let _ = classify;
# Ok(())
# }
```

---

### `prelude`

```rust
pub mod prelude { /* re-exports */ }
```

Brings the common surface — `Lsm`, `LsmConfig`, `Batch`, `Scan`, `Error`,
`Result` — into scope in one `use`.

```rust
use lsm_db::prelude::*;

fn main() -> Result<()> {
    let dir = tempfile::tempdir().map_err(Error::from)?;
    let db = Lsm::open(dir.path())?;
    db.put(b"k", b"v")?;
    Ok(())
}
```

---

### `format`

```rust
pub mod format { /* RunWriter, RunReader, RunIter, Entry, crc32c, MAX_KEY_LEN, MAX_VALUE_LEN */ }
```

The sorted-run format of
[`docs/SSTABLE_FORMAT.md`](./SSTABLE_FORMAT.md) as an in-memory codec, over
byte buffers instead of files. It is the part of the crate that works without
`std`: `crc32c`, `Entry`, and the limits need nothing; `RunWriter` and
`RunReader` need the `alloc` feature (implied by `std`). A run built with
`RunWriter` is byte-for-byte what the engine writes for the same entries, and
`RunReader` reads the run files the engine writes, so runs can move between a
host and a device in either direction. *(since 1.1)*

#### `RunWriter`

```rust
pub struct RunWriter { /* ... */ }

impl RunWriter {
    pub fn new() -> RunWriter;
    pub fn put(&mut self, key: impl AsRef<[u8]>, value: impl AsRef<[u8]>) -> Result<()>;
    pub fn delete(&mut self, key: impl AsRef<[u8]>) -> Result<()>;
    pub fn entry_count(&self) -> u64;
    pub fn finish(self) -> Vec<u8>;
}
```

Builds a run in memory. Add entries with `put` (a value) and `delete` (a
tombstone) in **strictly ascending** key order, then `finish` for the encoded
run. An out-of-order or duplicate key, or a key or value over the limit, returns
`Error::InvalidInput` and leaves the writer unchanged. `RunWriter` is also
`Default`, `Clone`, and `Debug`.

```rust
use lsm_db::format::{Entry, RunReader, RunWriter};

let mut writer = RunWriter::new();
writer.put(b"apple", b"red")?;
writer.delete(b"banana")?;
assert!(writer.put(b"aardvark", b"x").is_err()); // out of order
let bytes = writer.finish();

let run = RunReader::new(&bytes)?;
assert_eq!(run.get(b"banana")?, Some(Entry::Tombstone));
# Ok::<(), lsm_db::Error>(())
```

#### `RunReader`

```rust
pub struct RunReader<'a> { /* ... */ }

impl<'a> RunReader<'a> {
    pub fn new(bytes: &'a [u8]) -> Result<RunReader<'a>>;
    pub fn entry_count(&self) -> u64;
    pub fn get(&self, key: &[u8]) -> Result<Option<Entry<'a>>>;
    pub fn iter(&self) -> RunIter<'a, '_>;
}
```

A validated run held in memory. `new` checks the header and footer magic, the
index extent, the index checksum, and every index entry (order and block
extents), returning `Error::Corruption` on any failure. `get` reads, verifies,
and scans the one block that could hold the key. `iter` (also `&RunReader:
IntoIterator`) yields `Result<(&'a [u8], Entry<'a>)>` in key order, verifying
each block's checksum as it reaches it, and also checks that keys keep
increasing across blocks and that each block ends with the key the index
records; it yields the first error and then ends. Keys and values borrow from
the buffer; nothing is copied. `entry_count` is the footer's informational
count.

```rust
use lsm_db::format::{RunReader, RunWriter};

let mut writer = RunWriter::new();
for i in 0..1_000u32 {
    writer.put(format!("k{i:04}"), i.to_le_bytes())?;
}
let bytes = writer.finish();

let run = RunReader::new(&bytes)?;
assert_eq!(run.entry_count(), 1_000);
assert_eq!(run.iter().filter(|r| r.is_ok()).count(), 1_000);
assert!(run.get(b"k9999")?.is_none());
# Ok::<(), lsm_db::Error>(())
```

#### `Entry`

```rust
pub enum Entry<'a> {
    Value(&'a [u8]),
    Tombstone,
}
```

What a key maps to in one run. A `Tombstone` records a deletion and must shadow
older runs. `value()` returns `Option<&'a [u8]>` and `is_tombstone()` tells the
two apart. `Copy`, `Eq`, `Hash`, and `Debug`.

#### `crc32c`

```rust
pub fn crc32c(data: &[u8]) -> u32
```

The CRC32C (Castagnoli) checksum the format uses. With `std` it runs on the
`crc32c` crate, which picks the SSE4.2 or ARMv8 CRC instructions at run time.
Without `std` it uses those instructions when the target enables them at compile
time (`-C target-feature=+sse4.2` on x86_64, `+crc` on aarch64), and
slicing-by-8 tables otherwise. Every path returns the same value.

```rust
assert_eq!(lsm_db::format::crc32c(b"123456789"), 0xE306_9283);
```

#### `MAX_KEY_LEN` & `MAX_VALUE_LEN`

```rust
pub const MAX_KEY_LEN: usize = 1 << 30;
pub const MAX_VALUE_LEN: usize = 1 << 30;
```

The longest key and value the format stores (1 GiB each). `Lsm::put`,
`Lsm::delete`, `Lsm::write`, and `RunWriter` reject anything longer with
`Error::InvalidInput`.

---

## Concurrency

`Lsm` is `Send + Sync` and every method takes `&self`, so one engine can be
wrapped in an [`Arc`](https://doc.rust-lang.org/std/sync/struct.Arc.html) and
used from many threads. Reads proceed in parallel; writes are serialized;
[`scan`](#lsmscan) returns a consistent snapshot and never blocks writers for
the duration of iteration. A background thread compacts runs as they accumulate;
its expensive merge runs with no lock held, taking the engine lock only to swap
the finished run in, so it does not block reads or writes for the merge. Dropping
the `Lsm` stops and joins that thread, then flushes the buffer (since 1.2.1;
before that a drop discarded unflushed writes in a build without
`durability`). A flush error at drop cannot be reported; call
[`flush`](#lsmflush) first to see it.

```rust
# fn main() -> Result<(), Box<dyn std::error::Error>> {
use std::sync::Arc;
use std::thread;
use lsm_db::Lsm;

let dir = tempfile::tempdir()?;
let db = Arc::new(Lsm::open(dir.path())?);

let writer = {
    let db = Arc::clone(&db);
    thread::spawn(move || -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        for i in 0..100u32 {
            db.put(format!("k{i:03}").into_bytes(), b"v")?;
        }
        Ok(())
    })
};
writer.join().expect("writer thread")?;
assert_eq!(db.scan(..)?.count(), 100);
# Ok(())
# }
```

---

## Durability & persistence

Data becomes durable when it is flushed: [`flush`](#lsmflush), or an automatic
flush when the buffer reaches its [capacity](#lsmconfig). A flush writes a new
run to a temporary file, `fsync`s it, atomically renames it into place, and
records it in the manifest — also written atomically. Compaction installs its
merged run the same way. The manifest is the source of truth for the live run
set, so a crash mid-flush or mid-compaction recovers cleanly: on open, temporary
files and run files the manifest does not name are reclaimed as orphans. The
byte-level format is frozen for 1.x and specified in
[`docs/SSTABLE_FORMAT.md`](./SSTABLE_FORMAT.md).

### Crash-safe writes (`durability` feature)

By default, writes are durable once flushed; a write still buffered in the
memtable when the process exits is lost. Enable the `durability` feature to close
that gap:

```toml
[dependencies]
lsm-db = { version = "1.3", features = ["durability"] }
```

With it on, every `put` / `delete` / `write` is appended to a `wal-db`
write-ahead log and `fsync`ed **before** it is acknowledged, and a batch is
logged as one atomic record. On open, the log is replayed into the memtable and
checkpointed to a run, so no acknowledged write is lost across a crash — even one
before the next flush. The log holds only the writes since the last flush; a
flush empties it. The public API is identical either way, so the same code runs
durably or not depending on the feature:

```rust
# fn main() -> Result<(), Box<dyn std::error::Error>> {
let dir = tempfile::tempdir()?;
{
    let db = lsm_db::Lsm::open(dir.path())?;
    db.put(b"k", b"v")?;   // logged + fsynced before returning (with `durability`)
    // ...process exits here without an explicit flush...
}
// Reopen: the write is recovered from the log.
let db = lsm_db::Lsm::open(dir.path())?;
assert_eq!(db.get(b"k")?, Some(b"v".to_vec()));
# Ok(())
# }
```

The durable write path is currently serial — each write holds the engine lock
across its `fsync` — so it trades throughput for the guarantee; batched group
commit is a later optimisation.

### Bloom-filtered reads (`bloom` feature)

A point lookup that misses the memtable has to consult the on-disk runs. Enable
the `bloom` feature to give each run a bloom filter over its keys, so a lookup
skips any run whose filter rejects the key — without reading a single data
block:

```toml
[dependencies]
lsm-db = { version = "1.3", features = ["bloom"] }
```

The win is on negative lookups across many runs: in a benchmark of misses over
16 runs this cut a lookup from ~280 µs to ~3 µs. Filters never produce false
negatives, so skipping a run they reject is always safe; a false positive merely
falls through to a normal, correct lookup. The public API is identical with or
without the feature.

Because the on-disk run format is frozen for the 1.x series, the filter is not
embedded in the run — it lives in a **sidecar** file (`<run>.sst.bloom`) written
when the run is created and loaded when it is reopened. A sidecar is a pure
acceleration hint: if it is missing or unreadable, the run is consulted directly
with identical results.

```rust
# fn main() -> Result<(), Box<dyn std::error::Error>> {
let dir = tempfile::tempdir()?;
let db = lsm_db::Lsm::open(dir.path())?;
db.put(b"present", b"1")?;
db.flush()?;
// With `bloom`, this miss is answered from the filter, touching no data block.
assert_eq!(db.get(b"absent")?, None);
# Ok(())
# }
```

---

## Feature flags

| Feature | Default | Description |
|---------|---------|-------------|
| `std` | yes | Standard library. The engine requires it. Implies `alloc`; adds `Error::Io`, the `error-forge` integration, and run-time CRC32C hardware detection. |
| `alloc` | via `std` | Heap allocation without `std`: [`RunWriter`](#runwriter) and [`RunReader`](#runreader). *(since 1.1)* |
| `durability` | no | Crash-safe writes via a `wal-db` write-ahead log. Implies `std`. See [above](#crash-safe-writes-durability-feature). |
| `bloom` | no | Per-run bloom filters that skip runs on point reads. Implies `std`. See [above](#bloom-filtered-reads-bloom-feature). |

All features are additive: enabling one never removes functionality.

---

## `no_std`

With `default-features = false` the crate builds on bare-metal targets (CI
builds `thumbv7em-none-eabihf`, `riscv32imac-unknown-none-elf`, and
`aarch64-unknown-none`). The engine needs a filesystem and stays behind `std`;
the [`format`](#format) codec does not.

```toml
[dependencies]
# Run codec, needs a global allocator.
lsm-db = { version = "1.3", default-features = false, features = ["alloc"] }
# Or only crc32c, Entry, the limits, and Error, with no allocator at all.
lsm-db = { version = "1.3", default-features = false }
```

The checksum is part of the format, so it is identical in every build. Without
`std` there is no run-time CPU detection, so build with the target feature to
get the hardware path:

```sh
RUSTFLAGS="-C target-feature=+crc" cargo build --target aarch64-unknown-none --no-default-features --features alloc
```

`unsafe` is forbidden in every build except a `no_std` one whose target enables
the CRC instructions, which calls those intrinsics in four one-line blocks in
`src/crc.rs`. Their only requirement is CPU support for the instruction, which
the compile-time target feature guarantees.

---

<sub>Copyright &copy; 2026 <strong>James Gober</strong>. All rights reserved.</sub>
