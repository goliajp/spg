//! v7.7.3 — embedded throughput baselines. Numbers from this
//! file feed the README QPS table. Run with:
//!
//!   cargo bench -p spg-embedded --bench embedded
//!
//! All benches use `open_in_memory` to isolate engine cost
//! from filesystem variance. The persistent fsync number is
//! measured by `insert_persistent_fsync` separately.

use criterion::{BatchSize, Criterion, Throughput, criterion_group, criterion_main};
use spg_embedded::{Database, spg_row};
use std::time::Duration;

spg_row! {
    struct User {
        id: i32,
        name: String,
    }
}

fn bench_insert_in_memory(c: &mut Criterion) {
    let mut group = c.benchmark_group("insert_in_memory");
    group.throughput(Throughput::Elements(1));
    group.measurement_time(Duration::from_secs(3));
    group.bench_function("single_row", |b| {
        let mut db = Database::open_in_memory();
        db.execute("CREATE TABLE u (id INT NOT NULL, name TEXT)")
            .unwrap();
        let mut n: i64 = 0;
        b.iter(|| {
            n += 1;
            let sql = format!("INSERT INTO u VALUES ({n}, 'alice')");
            db.execute(&sql).unwrap();
        });
    });
    group.finish();
}

fn bench_select_by_pk(c: &mut Criterion) {
    let mut db = Database::open_in_memory();
    db.execute("CREATE TABLE u (id INT NOT NULL, name TEXT)")
        .unwrap();
    db.execute("CREATE INDEX u_pk ON u (id)").unwrap();
    db.with_transaction(|tx| {
        for i in 0..10_000 {
            tx.execute(&format!("INSERT INTO u VALUES ({i}, 'name')"))?;
        }
        Ok::<_, spg_embedded::EngineError>(())
    })
    .unwrap();
    let mut group = c.benchmark_group("select_in_memory");
    group.throughput(Throughput::Elements(1));
    group.measurement_time(Duration::from_secs(3));
    let mut counter: i32 = 0;
    group.bench_function("pk_seek", |b| {
        b.iter(|| {
            counter = (counter + 1) % 10_000;
            let sql = format!("SELECT id, name FROM u WHERE id = {counter}");
            let rows: Vec<User> = db.query_typed(&sql).unwrap();
            assert_eq!(rows.len(), 1);
        });
    });
    group.finish();
}

fn bench_select_range(c: &mut Criterion) {
    let mut db = Database::open_in_memory();
    db.execute("CREATE TABLE t (id INT NOT NULL, v INT)").unwrap();
    db.execute("CREATE INDEX t_pk ON t (id)").unwrap();
    db.with_transaction(|tx| {
        for i in 0..10_000 {
            tx.execute(&format!("INSERT INTO t VALUES ({i}, {})", i * 3))?;
        }
        Ok::<_, spg_embedded::EngineError>(())
    })
    .unwrap();
    let mut group = c.benchmark_group("select_range");
    group.throughput(Throughput::Elements(100));
    group.bench_function("100_rows", |b| {
        b.iter_batched(
            || 0i32,
            |_| {
                let rows = db
                    .query("SELECT id FROM t WHERE id >= 500 AND id < 600")
                    .unwrap();
                assert_eq!(rows.len(), 100);
            },
            BatchSize::SmallInput,
        );
    });
    group.finish();
}

fn bench_vector_knn(c: &mut Criterion) {
    let mut db = Database::open_in_memory();
    db.execute("CREATE TABLE docs (id INT NOT NULL, emb VECTOR(8) NOT NULL)")
        .unwrap();
    db.execute("CREATE INDEX docs_emb ON docs USING hnsw (emb)").unwrap();
    db.with_transaction(|tx| {
        for i in 0..2_000 {
            let v: Vec<f32> = (0..8).map(|j| ((i + j) as f32) * 0.01).collect();
            let vec_lit = format!(
                "[{}]",
                v.iter().map(f32::to_string).collect::<Vec<_>>().join(",")
            );
            tx.execute(&format!("INSERT INTO docs VALUES ({i}, {vec_lit})"))?;
        }
        Ok::<_, spg_embedded::EngineError>(())
    })
    .unwrap();
    let mut group = c.benchmark_group("vector_knn");
    group.throughput(Throughput::Elements(1));
    group.bench_function("k10_dim8", |b| {
        b.iter(|| {
            let rows = db
                .query("SELECT id FROM docs ORDER BY emb <-> [0.1,0.2,0.3,0.4,0.5,0.6,0.7,0.8] LIMIT 10")
                .unwrap();
            assert_eq!(rows.len(), 10);
        });
    });
    group.finish();
}

fn bench_insert_persistent_fsync(c: &mut Criterion) {
    let mut path = std::env::temp_dir();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    path.push(format!("spg-bench-fsync-{nanos}-{}.db", std::process::id()));
    let mut db = Database::open_path(&path).unwrap();
    db.execute("CREATE TABLE t (id INT NOT NULL, name TEXT)")
        .unwrap();
    let mut group = c.benchmark_group("insert_persistent_fsync");
    group.throughput(Throughput::Elements(1));
    group.measurement_time(Duration::from_secs(3));
    let mut n: i64 = 0;
    group.bench_function("single_row", |b| {
        b.iter(|| {
            n += 1;
            db.execute(&format!("INSERT INTO t VALUES ({n}, 'a')"))
                .unwrap();
        });
    });
    group.finish();
    drop(db);
    let _ = std::fs::remove_file(&path);
    let mut wal = path.clone();
    wal.set_extension("db.wal");
    let _ = std::fs::remove_file(&wal);
}

criterion_group!(
    benches,
    bench_insert_in_memory,
    bench_select_by_pk,
    bench_select_range,
    bench_vector_knn,
    bench_insert_persistent_fsync,
);
criterion_main!(benches);
