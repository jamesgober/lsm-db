//! The paths that checksum every block they touch: writing a run (flush) and
//! opening a database that holds runs (recovery: every index block and bloom
//! sidecar is verified). Used to measure the CRC32C implementation's share.

use std::hint::black_box;

use criterion::{BatchSize, Criterion, criterion_group, criterion_main};
use lsm_db::{Lsm, LsmConfig};

const KEYS: u32 = 20_000;

fn key(i: u32) -> [u8; 8] {
    let mut k = *b"key00000";
    k[4..8].copy_from_slice(&i.to_be_bytes());
    k
}

fn bench_flush(c: &mut Criterion) {
    let value = [7u8; 100];
    c.bench_function("flush_20k_values_of_100_bytes", |b| {
        b.iter_batched(
            || {
                let dir = tempfile::tempdir().expect("tempdir");
                let db = Lsm::open_with(dir.path(), LsmConfig::new().memtable_capacity(64 << 20))
                    .expect("open");
                for i in 0..KEYS {
                    db.put(key(i), value).expect("put");
                }
                (dir, db)
            },
            |(dir, db)| {
                db.flush().expect("flush");
                black_box(&db);
                drop(db);
                drop(dir);
            },
            BatchSize::PerIteration,
        );
    });
}

fn bench_open(c: &mut Criterion) {
    let dir = tempfile::tempdir().expect("tempdir");
    {
        let db =
            Lsm::open_with(dir.path(), LsmConfig::new().memtable_capacity(64 << 20)).expect("open");
        let value = [7u8; 100];
        for run in 0..8u32 {
            for i in 0..KEYS / 8 {
                db.put(key(run * KEYS + i), value).expect("put");
            }
            db.flush().expect("flush");
        }
    }
    c.bench_function("open_with_8_runs", |b| {
        b.iter(|| black_box(Lsm::open(dir.path()).expect("open")));
    });
    let db = Lsm::open(dir.path()).expect("open");
    let mut i = 0u32;
    c.bench_function("get_spread_over_8_runs", |b| {
        b.iter(|| {
            i = (i + 7_919) % KEYS;
            let run = i % 8;
            black_box(db.get(key(run * KEYS + i / 8)).expect("get"))
        });
    });
}

criterion_group!(benches, bench_flush, bench_open);
criterion_main!(benches);
