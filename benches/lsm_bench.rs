//! Criterion benchmarks for the hot paths: point write, point read (hit and
//! miss), and full scan. Run with `cargo bench`; baselines are tracked over time
//! so a regression beyond the project threshold blocks a merge.

use std::hint::black_box;

use criterion::{BatchSize, Criterion, criterion_group, criterion_main};
use lsm_db::{Lsm, LsmConfig};

/// Build a database pre-loaded with `n` flushed keys, returning the temp dir
/// (kept alive) and the engine.
fn loaded(n: u32) -> (tempfile::TempDir, Lsm) {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = Lsm::open_with(dir.path(), LsmConfig::new().memtable_capacity(1 << 20)).expect("open");
    for i in 0..n {
        db.put(key(i), b"value-payload-of-modest-size")
            .expect("put");
    }
    db.flush().expect("flush");
    (dir, db)
}

#[inline]
fn key(i: u32) -> [u8; 8] {
    let mut k = *b"key00000";
    let digits = i.to_le_bytes();
    k[4..8].copy_from_slice(&digits);
    k
}

fn bench_put(c: &mut Criterion) {
    c.bench_function("put_into_memtable", |b| {
        b.iter_batched(
            || {
                let dir = tempfile::tempdir().expect("tempdir");
                // Large buffer so the timed put never triggers a flush.
                let db = Lsm::open_with(dir.path(), LsmConfig::new().memtable_capacity(1 << 30))
                    .expect("open");
                (dir, db, 0u32)
            },
            |(dir, db, mut i)| {
                i = i.wrapping_add(1);
                db.put(key(black_box(i)), black_box(b"value")).expect("put");
                (dir, db, i)
            },
            BatchSize::SmallInput,
        );
    });
}

fn bench_get_hit(c: &mut Criterion) {
    let (_dir, db) = loaded(10_000);
    c.bench_function("get_hit_from_run", |b| {
        let mut i = 0u32;
        b.iter(|| {
            i = (i + 1) % 10_000;
            black_box(db.get(black_box(key(i))).expect("get"));
        });
    });
}

fn bench_get_miss(c: &mut Criterion) {
    let (_dir, db) = loaded(10_000);
    c.bench_function("get_miss_from_run", |b| {
        b.iter(|| {
            black_box(db.get(black_box(b"absent-key-not-present")).expect("get"));
        });
    });
}

fn bench_scan(c: &mut Criterion) {
    let (_dir, db) = loaded(10_000);
    c.bench_function("scan_full_10k", |b| {
        b.iter(|| {
            let n = db.scan(black_box(..)).expect("scan").count();
            black_box(n);
        });
    });
}

criterion_group!(
    benches,
    bench_put,
    bench_get_hit,
    bench_get_miss,
    bench_scan
);
criterion_main!(benches);
