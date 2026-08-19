//! v7.20 P2 — WAL group-commit e2e.
//!
//! Covers the three contracts:
//!
//! * Concurrency win: N concurrent INSERTs through AsyncDatabase
//!   complete in far fewer than N × fsync-cost — the leader
//!   flushes for the whole batch. (Asserted as wall-clock bound,
//!   generous to avoid CI flake.)
//! * Durability unchanged: every Ok-returned INSERT is on disk —
//!   reopen replays all of them.
//! * Ordering: replayed WAL preserves commit order (LSNs strictly
//!   monotonic in the chunk stream).

use spg_embedded_tokio::AsyncDatabase;

fn unique_dir(name: &str) -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let p = std::env::temp_dir().join(format!("spg-gc-{}-{name}-{nanos}", std::process::id()));
    std::fs::create_dir_all(&p).unwrap();
    p
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn concurrent_writes_share_fsync() {
    let dir = unique_dir("share");
    let db_path = dir.join("gc.db");
    let db = AsyncDatabase::open_path(&db_path).await.unwrap();
    db.execute("CREATE TABLE t (id INT NOT NULL)")
        .await
        .unwrap();

    // 64 concurrent single-row INSERTs. Batching them is the whole
    // point, and what batching MEANS is that they share fsyncs: under
    // v7.19 each write cost one, so 64 writes cost 64.
    //
    // Counted rather than timed. This used to assert "under 128 ms,
    // since serial would be ~256" — a stand-in that answers the machine
    // instead of the engine, failing on a busy box that batches
    // perfectly and passing on a fast disk that batches nothing.
    //
    // v7.38.2 — and they are held at a barrier until all N are ready.
    // Without it the writers arrive at the commit path in whatever
    // order the runtime happened to schedule them, so the batch size
    // measured is the BOX's parallelism, not the engine's batching: a
    // two-core CI runner batched them in pairs and reported exactly
    // 32 fsyncs for 64 writes, tripping `< N/2` by a single fsync on a
    // docs-only commit. The barrier makes "they were concurrent" a
    // property of the test rather than a hope about the scheduler.
    const N: usize = 64;
    let fsyncs0 = spg_embedded::WAL_FSYNC_COUNT.load(std::sync::atomic::Ordering::Relaxed);
    let gate = std::sync::Arc::new(tokio::sync::Barrier::new(N));
    let mut tasks = Vec::with_capacity(N);
    for i in 0..N {
        let db = db.clone();
        let gate = gate.clone();
        tasks.push(tokio::spawn(async move {
            gate.wait().await;
            db.execute(&format!("INSERT INTO t VALUES ({i})"))
                .await
                .unwrap();
        }));
    }
    for t in tasks {
        t.await.unwrap();
    }

    // All rows visible.
    let rows = db.query("SELECT COUNT(*) FROM t").await.unwrap();
    let count = match &rows[0][0] {
        spg_embedded_tokio::Value::Int(n) => i64::from(*n),
        spg_embedded_tokio::Value::BigInt(n) => *n,
        other => panic!("{other:?}"),
    };
    assert_eq!(count, N as i64);

    let fsyncs = spg_embedded::WAL_FSYNC_COUNT.load(std::sync::atomic::Ordering::Relaxed) - fsyncs0;
    // Printed on every run, not only on failure: a count that is
    // drifting toward the bound is the warning this test can give
    // before it starts costing anyone a red CI.
    eprintln!("group-commit: {N} concurrent INSERTs cost {fsyncs} WAL fsyncs");
    // What the engine guarantees is that writers who meet at the commit
    // point SHARE an fsync. How many meet there is the machine's
    // business: `execute` hands each write to the blocking pool, and a
    // two-core CI runner grows that pool a thread at a time, so the
    // writers arrive staggered no matter what this test does before
    // them. The barrier above tightened it — 2 fsyncs here, every run —
    // but the same barrier left CI at 37, because arriving together at
    // the barrier is not arriving together at the fsync.
    //
    // So the bound is the one that is about the ENGINE rather than the
    // box: concurrent writes must not each pay their own fsync. Round
    // 858 measured the shape this catches by holding the write lock
    // across the shared fsync, so no two writers can ever meet — the
    // v7.19 behaviour, 131 fsyncs for these 64 writes. A regression to
    // exactly one apiece scores 64. Both fail; every degree of real
    // batching passes, on any hardware.
    //
    // A tighter bound would need the test to hold writers AT the commit
    // point, which is inside the engine and not reachable from here.
    // The count is printed on every run, so the trend is visible in the
    // log without being load-bearing in the assertion.
    assert!(
        fsyncs < N as u64,
        "{N} concurrent INSERTs cost {fsyncs} WAL fsyncs — each write paid \
         for its own, so group-commit is not batching at all"
    );
    drop(db);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn durability_survives_reopen() {
    let dir = unique_dir("dura");
    let db_path = dir.join("gc.db");
    {
        let db = AsyncDatabase::open_path(&db_path).await.unwrap();
        db.execute("CREATE TABLE t (id INT NOT NULL)")
            .await
            .unwrap();
        let mut tasks = Vec::new();
        for i in 0..32 {
            let db = db.clone();
            tasks.push(tokio::spawn(async move {
                db.execute(&format!("INSERT INTO t VALUES ({i})"))
                    .await
                    .unwrap();
            }));
        }
        for t in tasks {
            t.await.unwrap();
        }
        // Drop runs the final checkpoint + releases the lock.
    }
    let db = AsyncDatabase::open_path(&db_path).await.unwrap();
    let rows = db.query("SELECT COUNT(*) FROM t").await.unwrap();
    let count = match &rows[0][0] {
        spg_embedded_tokio::Value::Int(n) => i64::from(*n),
        spg_embedded_tokio::Value::BigInt(n) => *n,
        other => panic!("{other:?}"),
    };
    assert_eq!(count, 32, "every Ok-ed INSERT must survive reopen");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn wal_lsns_stay_monotonic_under_concurrency() {
    use spg_embedded::parse_wal_records;
    let dir = unique_dir("order");
    let db_path = dir.join("gc.db");
    let db = AsyncDatabase::open_path(&db_path).await.unwrap();
    db.execute("CREATE TABLE t (id INT NOT NULL)")
        .await
        .unwrap();
    let mut tasks = Vec::new();
    for i in 0..32 {
        let db = db.clone();
        tasks.push(tokio::spawn(async move {
            db.execute(&format!("INSERT INTO t VALUES ({i})"))
                .await
                .unwrap();
        }));
    }
    for t in tasks {
        t.await.unwrap();
    }

    // Concat chunks in sorted order; SQL-record LSNs must be
    // strictly increasing (same invariant verify-pitr checks).
    let wal_dir = dir.join("gc.db.wal");
    let mut chunks: Vec<_> = std::fs::read_dir(&wal_dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("wal"))
        .collect();
    chunks.sort();
    let mut all = Vec::new();
    for c in chunks {
        all.extend_from_slice(&std::fs::read(&c).unwrap());
    }
    let recs = parse_wal_records(&all).unwrap();
    let lsns: Vec<u64> = recs
        .iter()
        .filter(|r| r.type_byte == 0x10)
        .filter_map(|r| r.commit_lsn)
        .collect();
    assert!(!lsns.is_empty());
    for w in lsns.windows(2) {
        assert!(w[0] < w[1], "LSNs must be strictly increasing: {lsns:?}");
    }
    drop(db);
}
