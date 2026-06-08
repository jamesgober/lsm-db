<h1 align="center">
    <img width="99" alt="Rust logo" src="https://raw.githubusercontent.com/jamesgober/rust-collection/72baabd71f00e14aa9184efcb16fa3deddda3a0a/assets/rust-logo.svg">
    <br><b>lsm-db</b><br>
    <sub><sup>PERFORMANCE</sup></sub>
</h1>
<div align="center">
    <sup>
        <a href="../README.md" title="Project Home"><b>HOME</b></a>
        <span>&nbsp;│&nbsp;</span>
        <a href="./API.md" title="API Reference"><b>API</b></a>
        <span>&nbsp;│&nbsp;</span>
        <span>PERFORMANCE</span>
    </sup>
</div>
<br>

> Honest, reproducible numbers. Every figure here comes from a `criterion`
> benchmark in [`benches/`](../benches); run `cargo bench` to reproduce them on
> your own hardware. These are micro-benchmarks — they characterise specific
> operations, not a full application — and absolute numbers vary by machine.
>
> **These are the locked benchmark baselines for the 1.0 line** (confirmed at the
> `0.9.0` beta). The engine's hot paths are unchanged since the block cache
> landed in `0.6`; a regression beyond 5% on any tracked metric blocks a release.

## Reproducing

```sh
cargo bench --bench lsm_bench                          # lsm-db hot paths
cargo bench --bench lsm_bench --features bloom         # with bloom filters
cargo bench --bench comparison                         # vs sled and redb
```

The numbers below were taken on Windows 11 (x86_64), Rust stable, release
profile, over a 10,000-key working set with 8-byte keys and a ~40-byte value.
They will differ on your hardware; the **ratios** are the durable signal.

---

## Bloom filters cut negative lookups

A point lookup that misses must consult each on-disk run. A per-run bloom filter
(the [`bloom`](./API.md#bloom-filtered-reads-bloom-feature) feature) lets a miss
skip a run with no data-block read. Over a key absent from 16 runs, **with the
block cache disabled** to isolate the filter's effect:

| | negative lookup | speedup |
|---|---|---|
| without `bloom` | ~280 µs | — |
| with `bloom` | ~3 µs | **~90×** |

A CI-enforced test additionally asserts that such a lookup reads **zero** data
blocks. (With the block cache on — the default — repeat negative lookups are
already fast because their candidate blocks are cached; the filter's full value
shows on cold lookups and working sets larger than the cache.)

---

## The block cache cuts repeat reads

The block cache (on by default, 8 MiB) keeps decoded run blocks so a repeat point
lookup over a hot working set does no I/O, checksum, or parse. A CI-enforced test
asserts that a second lookup of the same key reads **zero** data blocks, while a
lookup with the cache disabled reads one. The cache is shared across runs and
uses sharded CLOCK eviction.

---

## Versus `sled` and `redb`

A fair-shape comparison — identical keys, values, and counts — against two mature
pure-Rust embedded key-value stores. Each engine makes different durability and
structural tradeoffs, so this is a characterisation, not a verdict.

| Operation | `lsm-db` | `sled` 0.34 | `redb` 2.6 |
|-----------|---------:|------------:|-----------:|
| Point read (hit) | **125 ns** | 215 ns | 156 ns |
| Bulk insert (10k) | **11.0 ms** | 24.9 ms | 22.9 ms |
| Full scan (10k) | 1.80 ms | 1.61 ms | **0.39 ms** |

Reading the table honestly:

- **Point reads:** `lsm-db` is fastest. The lookup hits the in-memory block index,
  serves the block from cache, and (with `bloom`) skips runs that cannot hold the
  key.
- **Bulk insert:** `lsm-db` is roughly 2× faster. Writes land in the sorted
  memtable and flush as one sequential run — the LSM write path's whole point —
  versus the B-tree page updates `sled` and `redb` perform.
- **Full scan:** `redb` is markedly faster, and `lsm-db` is currently the
  slowest. `lsm-db`'s `scan` materialises a consistent snapshot of the range into
  a `Vec` before returning; `redb` streams its B-tree in place. Lazy,
  non-materialising scan streaming is the optimisation that closes this gap and
  is tracked for a future release — the snapshot semantics it provides will not
  change.

The takeaway matches the design: an LSM tree is built for write-heavy and
point-read workloads, which is exactly where `lsm-db` leads; ordered range scans
are a B-tree's home turf.

---

<sub>Copyright &copy; 2026 <strong>James Gober</strong>. All rights reserved.</sub>
