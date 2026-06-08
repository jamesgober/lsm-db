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

/// Negative lookups across many runs. This is where bloom filters pay off:
/// without the `bloom` feature each run costs a candidate-block read; with it,
/// every run is skipped. Run `cargo bench --bench lsm_bench negative_lookup`
/// with and without `--features bloom` to see the difference.
fn bench_negative_lookup_many_runs(c: &mut Criterion) {
    let dir = tempfile::tempdir().expect("tempdir");
    // A high trigger keeps the flushed runs separate so the lookup must consult
    // all of them (absent the filter).
    let db = Lsm::open_with(dir.path(), LsmConfig::new().compaction_trigger(64)).expect("open");
    for run in 0..16u32 {
        for i in 0..2_000u32 {
            // Even keys present in every run; odd keys never present.
            db.put(key(i * 2), b"v").expect("put");
        }
        let _ = run;
        db.flush().expect("flush");
    }
    c.bench_function("negative_lookup_16_runs", |b| {
        let mut i = 1u32;
        b.iter(|| {
            i = (i + 2) % 4_000; // odd keys: always absent
            black_box(db.get(black_box(key(i))).expect("get"));
        });
    });
}

criterion_group!(
    benches,
    bench_put,
    bench_get_hit,
    bench_get_miss,
    bench_scan,
    bench_negative_lookup_many_runs
);
criterion_main!(benches);
