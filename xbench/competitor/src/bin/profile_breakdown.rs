//! v7.20 P1 — perf breakdown profiler.
//!
//! Isolates each stage of the write path and the read path so the
//! v7.20 perf epic optimises measured bottlenecks, not guesses.
//!
//! Write path stages (per single-row INSERT):
//!   1. engine-only        — in-memory execute(), no WAL
//!   2. + WAL encode       — encode_v4 record bytes (sim)
//!   3. + WAL write        — file write, NO fsync
//!   4. + fsync            — the full durable path (= file-backed execute)
//!
//! Read path stages (per PK SELECT):
//!   a. parse only         — parse_statement(sql)
//!   b. is_readonly_sql    — what SpgConnection routing pays per stmt
//!   c. prepare            — engine prepare (parse + transforms)
//!   d. snapshot clone     — Engine::clone_snapshot()
//!   e. readonly exec      — execute_readonly_on_snapshot (cached snapshot)
//!   f. spawn_blocking rtt — tokio spawn_blocking round-trip of a no-op
//!
//! Run: cargo run --release -p spg-bench-competitor --bin profile_breakdown

#![allow(
    clippy::cast_lossless,
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::doc_markdown,
    clippy::too_many_lines,
    clippy::uninlined_format_args,
    clippy::unused_io_amount
)]

use std::io::Write as _;
use std::time::Instant;

const ITERS: usize = 2000;

fn pcts(mut v: Vec<u64>) -> (u64, u64, u64) {
    v.sort_unstable();
    let p = |q: f64| v[((v.len() as f64 - 1.0) * q) as usize];
    (p(0.5), p(0.95), p(0.99))
}

fn main() {
    println!("# v7.20 perf breakdown — {ITERS} iters per stage, µs\n");

    // ---------- WRITE PATH ----------
    println!("## write path (single-row INSERT)\n");
    println!("| stage | p50 | p95 | p99 |");
    println!("|---|---:|---:|---:|");

    // 1. engine-only (in-memory).
    {
        use spg_engine::Engine;
        let mut eng = Engine::new();
        eng.execute("CREATE TABLE t (id INT NOT NULL, name TEXT NOT NULL)")
            .unwrap();
        let mut lat = Vec::with_capacity(ITERS);
        for i in 0..ITERS {
            let sql = format!("INSERT INTO t VALUES ({i}, 'user-{i}')");
            let t0 = Instant::now();
            eng.execute(&sql).unwrap();
            lat.push(t0.elapsed().as_micros() as u64);
        }
        let (a, b, c) = pcts(lat);
        println!("| engine-only (mem) | {a} | {b} | {c} |");
    }

    // 2-3. WAL write without fsync (raw file append of a ~64B record).
    {
        let tmp = std::env::temp_dir().join(format!("spg-prof-{}.wal", std::process::id()));
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&tmp)
            .unwrap();
        let record = vec![0u8; 64];
        let mut lat = Vec::with_capacity(ITERS);
        for _ in 0..ITERS {
            let t0 = Instant::now();
            f.write_all(&record).unwrap();
            lat.push(t0.elapsed().as_micros() as u64);
        }
        let (a, b, c) = pcts(lat);
        println!("| wal write (no fsync) | {a} | {b} | {c} |");
        let _ = std::fs::remove_file(&tmp);
    }

    // 4a. bare fsync cost on this host (the per-commit price).
    {
        let tmp = std::env::temp_dir().join(format!("spg-prof-sync-{}.wal", std::process::id()));
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&tmp)
            .unwrap();
        let record = vec![0u8; 64];
        let mut lat = Vec::with_capacity(ITERS.min(500));
        for _ in 0..ITERS.min(500) {
            f.write_all(&record).unwrap();
            let t0 = Instant::now();
            f.sync_data().unwrap();
            lat.push(t0.elapsed().as_micros() as u64);
        }
        let (a, b, c) = pcts(lat);
        println!("| fsync (sync_data) | {a} | {b} | {c} |");
        let _ = std::fs::remove_file(&tmp);
    }

    // 4b. full durable execute() through spg-embedded (file-backed).
    {
        use spg_embedded::Database;
        let tmpdir = std::env::temp_dir().join(format!("spg-prof-db-{}", std::process::id()));
        std::fs::create_dir_all(&tmpdir).unwrap();
        let db_path = tmpdir.join("p.db");
        let mut db = Database::open_path(&db_path).unwrap();
        db.execute("CREATE TABLE t (id INT NOT NULL, name TEXT NOT NULL)")
            .unwrap();
        let mut lat = Vec::with_capacity(ITERS.min(500));
        for i in 0..ITERS.min(500) {
            let sql = format!("INSERT INTO t VALUES ({i}, 'user-{i}')");
            let t0 = Instant::now();
            db.execute(&sql).unwrap();
            lat.push(t0.elapsed().as_micros() as u64);
        }
        let (a, b, c) = pcts(lat);
        println!("| durable execute (file+fsync) | {a} | {b} | {c} |");
        drop(db);
        let _ = std::fs::remove_dir_all(&tmpdir);
    }

    // 5. UPDATE-by-PK on a 5000-row table (the mixed-bench write
    //    shape) — sync engine only, isolates the mutation cost.
    {
        use spg_engine::Engine;
        let mut eng = Engine::new();
        eng.execute("CREATE TABLE b (id INT NOT NULL, v INT NOT NULL)")
            .unwrap();
        eng.execute("CREATE INDEX b_id ON b (id)").unwrap();
        for i in 0..5000 {
            eng.execute(&format!("INSERT INTO b VALUES ({i}, {i})"))
                .unwrap();
        }
        let mut lat = Vec::with_capacity(ITERS);
        for i in 0..ITERS {
            let sql = format!("UPDATE b SET v = v + 1 WHERE id = {}", i % 5000);
            let t0 = Instant::now();
            eng.execute(&sql).unwrap();
            lat.push(t0.elapsed().as_micros() as u64);
        }
        let (a, b, c) = pcts(lat);
        println!("| UPDATE-by-PK 5k rows (engine) | {a} | {b} | {c} |");
    }

    // 6. Same UPDATE through AsyncDatabase — full sqlx-shape
    //    write path INCLUDING the per-commit fsync (synchronous
    //    commit; the SPG_SYNCHRONOUS_COMMIT OnceLock was already
    //    pinned "on" by stage 4b, so this stage can't flip it).
    //    Gap vs stage 5 ≈ fsync + adapter overhead. Single-task
    //    serial, so group commit can't amortise here — see the
    //    mixed bench for the concurrent shape.
    {
        use spg_embedded_tokio::AsyncDatabase;
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .build()
            .unwrap();
        let tmpdir = std::env::temp_dir().join(format!("spg-prof-upd-{}", std::process::id()));
        std::fs::create_dir_all(&tmpdir).unwrap();
        let db_path = tmpdir.join("u.db");
        let mut lat = Vec::with_capacity(ITERS.min(1000));
        rt.block_on(async {
            let db = AsyncDatabase::open_path(&db_path).await.unwrap();
            db.execute("CREATE TABLE b (id INT NOT NULL, v INT NOT NULL)")
                .await
                .unwrap();
            db.execute("CREATE INDEX b_id ON b (id)").await.unwrap();
            for i in 0..5000 {
                db.execute(&format!("INSERT INTO b VALUES ({i}, {i})"))
                    .await
                    .unwrap();
            }
            for i in 0..ITERS.min(1000) {
                let sql = format!("UPDATE b SET v = v + 1 WHERE id = {}", i % 5000);
                let t0 = Instant::now();
                db.execute(&sql).await.unwrap();
                lat.push(t0.elapsed().as_micros() as u64);
            }
        });
        let (a, b, c) = pcts(lat);
        println!("| UPDATE-by-PK via AsyncDatabase | {a} | {b} | {c} |");
        let _ = std::fs::remove_dir_all(&tmpdir);
    }

    // ---------- READ PATH ----------
    println!("\n## read path (PK SELECT)\n");
    println!("| stage | p50 | p95 | p99 |");
    println!("|---|---:|---:|---:|");

    let sql = "SELECT id, name FROM t WHERE id = 42";

    // a. parse only.
    {
        let mut lat = Vec::with_capacity(ITERS);
        for _ in 0..ITERS {
            let t0 = Instant::now();
            let _ = spg_sql::parser::parse_statement(sql).unwrap();
            lat.push(t0.elapsed().as_micros() as u64);
        }
        let (a, b, c) = pcts(lat);
        println!("| parse_statement | {a} | {b} | {c} |");
    }

    // b. is_readonly_sql (per-statement routing cost in SpgConnection).
    {
        use spg_engine::Engine;
        let mut lat = Vec::with_capacity(ITERS);
        for _ in 0..ITERS {
            let t0 = Instant::now();
            let _ = Engine::is_readonly_sql(sql);
            lat.push(t0.elapsed().as_micros() as u64);
        }
        let (a, b, c) = pcts(lat);
        println!("| is_readonly_sql | {a} | {b} | {c} |");
    }

    // c-e. prepare / snapshot clone / readonly exec on a seeded engine.
    {
        use spg_engine::Engine;
        let mut eng = Engine::new();
        eng.execute("CREATE TABLE t (id INT NOT NULL, name TEXT NOT NULL)")
            .unwrap();
        eng.execute("CREATE INDEX t_id ON t (id)").unwrap();
        for i in 0..1000 {
            eng.execute(&format!("INSERT INTO t VALUES ({i}, 'user-{i}')"))
                .unwrap();
        }

        let mut lat = Vec::with_capacity(ITERS);
        for _ in 0..ITERS {
            let t0 = Instant::now();
            let _ = eng.prepare(sql).unwrap();
            lat.push(t0.elapsed().as_micros() as u64);
        }
        let (a, b, c) = pcts(lat);
        println!("| engine prepare | {a} | {b} | {c} |");

        let mut lat = Vec::with_capacity(ITERS);
        for _ in 0..ITERS {
            let t0 = Instant::now();
            let _ = eng.clone_snapshot();
            lat.push(t0.elapsed().as_micros() as u64);
        }
        let (a, b, c) = pcts(lat);
        println!("| clone_snapshot | {a} | {b} | {c} |");

        let snap = eng.clone_snapshot();
        let mut lat = Vec::with_capacity(ITERS);
        for _ in 0..ITERS {
            let t0 = Instant::now();
            let _ = Engine::execute_readonly_on_snapshot(&snap, sql).unwrap();
            lat.push(t0.elapsed().as_micros() as u64);
        }
        let (a, b, c) = pcts(lat);
        println!("| readonly exec (cached snap) | {a} | {b} | {c} |");
    }

    // f. spawn_blocking round-trip (tokio overhead per statement).
    {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .build()
            .unwrap();
        let mut lat = Vec::with_capacity(ITERS);
        rt.block_on(async {
            for _ in 0..ITERS {
                let t0 = Instant::now();
                tokio::task::spawn_blocking(|| 1 + 1).await.unwrap();
                lat.push(t0.elapsed().as_micros() as u64);
            }
        });
        let (a, b, c) = pcts(lat);
        println!("| spawn_blocking rtt | {a} | {b} | {c} |");
    }

    println!("\ndone.");
}
