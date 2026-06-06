<h1 align="center">
    <img width="99" alt="Rust logo" src="https://raw.githubusercontent.com/jamesgober/rust-collection/72baabd71f00e14aa9184efcb16fa3deddda3a0a/assets/rust-logo.svg">
    <br>
    <b>lsm-db</b>
    <br>
    <sub><sup>LSM-TREE STORAGE ENGINE</sup></sub>
</h1>

<div align="center">
    <a href="https://crates.io/crates/lsm-db"><img alt="Crates.io" src="https://img.shields.io/crates/v/lsm-db"></a>
    <a href="https://crates.io/crates/lsm-db" alt="Download lsm-db"><img alt="Crates.io Downloads" src="https://img.shields.io/crates/d/lsm-db?color=%230099ff"></a>
    <a href="https://docs.rs/lsm-db" title="lsm-db Documentation"><img alt="docs.rs" src="https://img.shields.io/docsrs/lsm-db"></a>
    <a href="https://github.com/jamesgober/lsm-db/actions"><img alt="GitHub CI" src="https://github.com/jamesgober/lsm-db/actions/workflows/ci.yml/badge.svg"></a>
    <a href="https://github.com/rust-lang/rfcs/blob/master/text/2495-min-rust-version.md" title="MSRV"><img alt="MSRV" src="https://img.shields.io/badge/MSRV-1.85%2B-blue"></a>
</div>

<br>

<div align="left">
    <p>
        <strong>lsm-db</strong> is a <b>log-structured merge-tree</b> storage engine: the write path that powers RocksDB, LevelDB, Cassandra, and ScyllaDB, packaged as a clean Rust library. Writes go to an in-memory memtable backed by a durable log; when the memtable fills it is flushed to an immutable sorted run on disk; background compaction merges those runs to keep reads fast and space bounded.
    </p>
    <p>
        It is built from the portfolio's own primitives rather than re-deriving them: durability comes from <code>wal-db</code>, point-read filtering from <code>bloom-lib</code>, and record framing from <code>pack-io</code>. That keeps the engine small and lets each primitive be audited and benchmarked once.
    </p>
    <p>
        The common case is <code>open</code> / <code>put</code> / <code>get</code> / <code>scan</code>. Compaction strategy, level sizing, and write-buffer tuning live behind a builder.
    </p>
    <br>
    <hr>
    <p>
        <strong>MSRV is 1.85+</strong> (Rust 2024 edition). Durable writes via wal-db. Background compaction. Bloom-filtered reads.
    </p>
    <blockquote>
        <strong>Status: pre-1.0, in active development.</strong> The on-disk SSTable format is being designed and frozen across the 0.x series; <code>1.0.0</code> freezes the format. See <a href="./CHANGELOG.md"><code>CHANGELOG.md</code></a> for detail.
    </blockquote>
</div>

<hr>
<br>

<h2>What it does</h2>

**Available now (`0.2`):**

- **Memtable** &mdash; in-memory sorted write buffer; flushes to an immutable sorted run when full
- **Durable flush** &mdash; runs are written atomically and `fsync`ed; flushed data survives reopening
- **Tombstone deletes** &mdash; deletes mask older values and resolve away on flush
- **Range scans** &mdash; merge the buffer and the on-disk run into one sorted stream
- **Grouped writes** &mdash; apply a batch atomically with respect to concurrent readers
- **Shared, thread-safe handle** &mdash; one engine, many threads, behind an `Arc`

**On the roadmap:**

- **Leveled SSTables + background compaction** &mdash; multiple runs, merged in the background (`0.3`)
- **Write-ahead logging** &mdash; crash-safe un-flushed writes via `wal-db`, under `durability` (`0.4`)
- **Bloom filters** &mdash; skip runs that can't contain a key, under `bloom` (`0.5`)
- **Pluggable comparator** &mdash; custom key ordering (`0.5`)


<br>

## Installation

```toml
[dependencies]
lsm-db = "0.2"
```

<br>

## Quick Start

```rust
use lsm_db::Lsm;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Open (or create) a database backed by a directory.
    let db = Lsm::open("my-db")?;

    // Keys and values are arbitrary bytes.
    db.put(b"user:1", b"alice")?;
    db.put(b"user:2", b"bob")?;

    // Point reads return owned values.
    assert_eq!(db.get(b"user:1")?, Some(b"alice".to_vec()));

    // Deletes mask the key.
    db.delete(b"user:1")?;
    assert_eq!(db.get(b"user:1")?, None);

    // Range scans walk keys in sorted order.
    db.put(b"user:1", b"alice")?;
    for (key, value) in db.scan(b"user:".to_vec()..b"user;".to_vec())? {
        println!("{} = {}", String::from_utf8_lossy(&key), String::from_utf8_lossy(&value));
    }

    // Force the buffer to disk; it will be there on the next open.
    db.flush()?;
    Ok(())
}
```

Tuning lives behind [`LsmConfig`](./docs/API.md#lsmconfig); grouped writes behind [`Batch`](./docs/API.md#batch). See [`docs/API.md`](./docs/API.md) for the full reference and the [`examples/`](./examples) directory for runnable programs.

<br>

## Status

This is the <code>v0.2.0</code> foundation release: the Tier-1 API (<code>open</code>/<code>put</code>/<code>get</code>/<code>delete</code>/<code>scan</code>) is implemented and tested over a single on-disk run. Multi-level compaction, durability, and bloom filters land across the rest of the 0.x series per the project roadmap and <a href="./docs/API.md"><code>docs/API.md</code></a>. The on-disk format is not yet frozen.

<hr>
<br>

## Where It Fits

`lsm-db` is a storage engine. It builds on:

- [`wal-db`](https://github.com/jamesgober/wal-db) &mdash; memtable durability and crash recovery
- [`bloom-lib`](https://github.com/jamesgober/bloom-lib) &mdash; SSTable point-read filtering
- [`pack-io`](https://github.com/jamesgober/pack-io) &mdash; on-disk record framing
- Hive DB &mdash; a candidate storage engine behind the `StorageEngine` trait

It stays foreign-compatible: usable standalone as an embedded key-value store.

<br>

## Cross-Platform Support

**Tier 1 Support:**
- Linux (x86_64, aarch64)
- macOS (x86_64, Apple Silicon)
- Windows (x86_64)

Behavior is verified on each target by the CI matrix.

<br>

## Contributing

Before opening a PR, `cargo fmt --all`, `cargo clippy --all-targets --all-features -- -D warnings`, and `cargo test --all-features` must be clean. Hot-path changes require a `criterion` benchmark; correctness-critical paths require property and/or `loom` tests.


<br>

<div id="license">
    <h2>License</h2>
    <p>Licensed under either of</p>
    <ul>
        <li><b>Apache License, Version 2.0</b> &mdash; see <a href="./LICENSE-APACHE">LICENSE-APACHE</a></li>
        <li><b>MIT License</b> &mdash; see <a href="./LICENSE-MIT">LICENSE-MIT</a></li>
    </ul>
    <p>at your option.</p>
</div>

<div align="center">
  <h2></h2>
  <sup>COPYRIGHT <small>&copy;</small> 2026 <strong>JAMES GOBER.</strong></sup>
</div>
