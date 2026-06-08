//! Comparative benchmark: `lsm-db` against two mature pure-Rust embedded
//! key-value stores, `sled` and `redb`, on the same workload.
//!
//! Run with `cargo bench --bench comparison`. The numbers it produces are
//! recorded honestly in `docs/PERFORMANCE.md`. This is a *fair-shape* comparison
//! — identical keys, values, and counts — not a claim that one engine is best
//! for every workload; each makes different durability and structure tradeoffs.

use std::hint::black_box;

use criterion::{BatchSize, Criterion, criterion_group, criterion_main};
use lsm_db::{Lsm, LsmConfig};
use redb::{Database, ReadableTable, TableDefinition};

/// Number of keys in the working set.
const N: u32 = 10_000;
/// A modest fixed value payload.
const VALUE: &[u8] = b"a-value-payload-of-modest-size-0123456789";
/// The redb table all rows live in.
const TABLE: TableDefinition<&[u8], &[u8]> = TableDefinition::new("kv");

#[inline]
fn key(i: u32) -> [u8; 8] {
    let mut k = *b"key00000";
    k[4..8].copy_from_slice(&i.to_be_bytes());
    k
}

fn load_lsm() -> (tempfile::TempDir, Lsm) {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = Lsm::open_with(dir.path(), LsmConfig::new().memtable_capacity(1 << 20)).expect("open");
    for i in 0..N {
        db.put(key(i), VALUE).expect("put");
    }
    db.flush().expect("flush");
    (dir, db)
}

fn load_sled() -> (tempfile::TempDir, sled::Db) {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = sled::open(dir.path()).expect("open");
    for i in 0..N {
        let _ = db.insert(key(i), VALUE).expect("insert");
    }
    db.flush().expect("flush");
    (dir, db)
}

fn load_redb() -> (tempfile::TempDir, Database) {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = Database::create(dir.path().join("data.redb")).expect("create");
    let wtx = db.begin_write().expect("begin_write");
    {
        let mut t = wtx.open_table(TABLE).expect("open_table");
        for i in 0..N {
            let _ = t.insert(key(i).as_slice(), VALUE).expect("insert");
        }
    }
    wtx.commit().expect("commit");
    (dir, db)
}

fn bench_get_hit(c: &mut Criterion) {
    let mut g = c.benchmark_group("get_hit_10k");

    let (_d1, lsm) = load_lsm();
    g.bench_function("lsm-db", |b| {
        let mut i = 0u32;
        b.iter(|| {
            i = (i + 1) % N;
            black_box(lsm.get(black_box(key(i))).expect("get"));
        });
    });

    let (_d2, sled) = load_sled();
    g.bench_function("sled", |b| {
        let mut i = 0u32;
        b.iter(|| {
            i = (i + 1) % N;
            black_box(sled.get(black_box(key(i))).expect("get"));
        });
    });

    let (_d3, redb) = load_redb();
    let rtx = redb.begin_read().expect("begin_read");
    let t = rtx.open_table(TABLE).expect("open_table");
    g.bench_function("redb", |b| {
        let mut i = 0u32;
        b.iter(|| {
            i = (i + 1) % N;
            black_box(t.get(black_box(key(i).as_slice())).expect("get"));
        });
    });

    g.finish();
}

fn bench_bulk_insert(c: &mut Criterion) {
    let mut g = c.benchmark_group("bulk_insert_10k");
    g.sample_size(20);

    g.bench_function("lsm-db", |b| {
        b.iter_batched(
            || tempfile::tempdir().expect("tempdir"),
            |dir| {
                let db = Lsm::open_with(dir.path(), LsmConfig::new().memtable_capacity(1 << 20))
                    .expect("open");
                for i in 0..N {
                    db.put(key(i), VALUE).expect("put");
                }
                db.flush().expect("flush");
                dir
            },
            BatchSize::SmallInput,
        );
    });

    g.bench_function("sled", |b| {
        b.iter_batched(
            || tempfile::tempdir().expect("tempdir"),
            |dir| {
                let db = sled::open(dir.path()).expect("open");
                for i in 0..N {
                    let _ = db.insert(key(i), VALUE).expect("insert");
                }
                db.flush().expect("flush");
                dir
            },
            BatchSize::SmallInput,
        );
    });

    g.bench_function("redb", |b| {
        b.iter_batched(
            || tempfile::tempdir().expect("tempdir"),
            |dir| {
                let db = Database::create(dir.path().join("data.redb")).expect("create");
                let wtx = db.begin_write().expect("begin_write");
                {
                    let mut t = wtx.open_table(TABLE).expect("open_table");
                    for i in 0..N {
                        let _ = t.insert(key(i).as_slice(), VALUE).expect("insert");
                    }
                }
                wtx.commit().expect("commit");
                dir
            },
            BatchSize::SmallInput,
        );
    });

    g.finish();
}

fn bench_full_scan(c: &mut Criterion) {
    let mut g = c.benchmark_group("full_scan_10k");

    let (_d1, lsm) = load_lsm();
    g.bench_function("lsm-db", |b| {
        b.iter(|| black_box(lsm.scan(black_box(..)).expect("scan").count()));
    });

    let (_d2, sled) = load_sled();
    g.bench_function("sled", |b| {
        b.iter(|| black_box(sled.iter().filter(Result::is_ok).count()));
    });

    let (_d3, redb) = load_redb();
    let rtx = redb.begin_read().expect("begin_read");
    let t = rtx.open_table(TABLE).expect("open_table");
    g.bench_function("redb", |b| {
        b.iter(|| {
            let mut n = 0usize;
            for row in t.iter().expect("iter") {
                let _ = row.expect("row");
                n += 1;
            }
            black_box(n)
        });
    });

    g.finish();
}

criterion_group!(
    comparison,
    bench_get_hit,
    bench_bulk_insert,
    bench_full_scan
);
criterion_main!(comparison);
