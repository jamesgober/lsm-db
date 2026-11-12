//! Range scans that cover a small part of the key space: the cost should
//! follow the range, not the keys before it. 100,000 keys in 8 runs, and
//! 50,000 in the memtable alone; each scan returns 100 keys.

use std::hint::black_box;

use criterion::{Criterion, criterion_group, criterion_main};
use lsm_db::{Lsm, LsmConfig};

fn key(i: u32) -> Vec<u8> {
    format!("k{i:06}").into_bytes()
}

fn bench_scan_range(c: &mut Criterion) {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = Lsm::open_with(
        dir.path(),
        LsmConfig::new()
            .memtable_capacity(1 << 30)
            .compaction_trigger(usize::MAX),
    )
    .expect("open");
    for run in 0..8u32 {
        for i in 0..12_500u32 {
            db.put(key(i * 8 + run), b"value-payload-of-modest-size")
                .expect("put");
        }
        db.flush().expect("flush");
    }
    let mut g = c.benchmark_group("scan_range");
    g.bench_function("100_of_100k_in_8_runs_at_start", |b| {
        b.iter(|| black_box(db.scan(key(0)..key(100)).expect("scan").count()));
    });
    g.bench_function("100_of_100k_in_8_runs_at_end", |b| {
        b.iter(|| black_box(db.scan(key(99_800)..key(99_900)).expect("scan").count()));
    });

    let mem_dir = tempfile::tempdir().expect("tempdir");
    let mem =
        Lsm::open_with(mem_dir.path(), LsmConfig::new().memtable_capacity(1 << 30)).expect("open");
    for i in 0..50_000u32 {
        mem.put(key(i), b"value-payload-of-modest-size")
            .expect("put");
    }
    g.bench_function("100_of_50k_in_memtable_at_end", |b| {
        b.iter(|| black_box(mem.scan(key(49_800)..key(49_900)).expect("scan").count()));
    });
    g.finish();
}

criterion_group!(benches, bench_scan_range);
criterion_main!(benches);
