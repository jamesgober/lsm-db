//! CRC32C throughput: `lsm_db::format::crc32c` against the `crc32c` crate.
//!
//! With the default features both sides are the `crc32c` crate, so the two
//! lines should match. The interesting runs are the `no_std` builds, which
//! measure the compile-time implementation in `src/crc.rs`:
//!
//! ```sh
//! cargo bench --bench crc                                          # std
//! cargo bench --bench crc --no-default-features --features alloc   # table
//! RUSTFLAGS="-C target-feature=+sse4.2" \
//!   cargo bench --bench crc --no-default-features --features alloc # hardware
//! ```

use std::hint::black_box;

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};

fn bench_crc(c: &mut Criterion) {
    let mut group = c.benchmark_group("crc32c");
    for len in [64usize, 4 * 1024, 64 * 1024] {
        let data: Vec<u8> = (0..len).map(|i| (i * 31 + 7) as u8).collect();
        let _ = group.throughput(Throughput::Bytes(len as u64));
        let _ = group.bench_with_input(BenchmarkId::new("lsm_db", len), &data, |b, d| {
            b.iter(|| lsm_db::format::crc32c(black_box(d)));
        });
        let _ = group.bench_with_input(BenchmarkId::new("crc32c_crate", len), &data, |b, d| {
            b.iter(|| crc32c::crc32c(black_box(d)));
        });
    }
    group.finish();
}

criterion_group!(benches, bench_crc);
criterion_main!(benches);
