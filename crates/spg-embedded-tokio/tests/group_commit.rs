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
use std::time::Instant;

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

    // 64 concurrent single-row INSERTs. Under v7.19 (one fsync
    // each, ~4 ms serial) this takes ≥ 64 × 4 ms ≈ 256 ms.
    // Under group-commit the writes batch: expect well under
    // half the serial bound. Generous threshold to absorb CI
    // noise — the real assertion is "not serial".
    const N: usize = 64;
    let t0 = Instant::now();
    let mut tasks = Vec::with_capacity(N);
    for i in 0..N {
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
    let elapsed = t0.elapsed();

    // All rows visible.
    let rows = db.query("SELECT COUNT(*) FROM t").await.unwrap();
    let count = match &rows[0][0] {
        spg_embedded_tokio::Value::Int(n) => i64::from(*n),
        spg_embedded_tokio::Value::BigInt(n) => *n,
        other => panic!("{other:?}"),
    };
    assert_eq!(count, N as i64);

    // Serial bound would be ≥ N × ~4 ms ≈ 256 ms on this host
    // (profile_breakdown). Group-commit should land well under
    // half of that. 128 ms keeps the assertion meaningful while
    // tolerating slow CI disks.
    assert!(
        elapsed.as_millis() < 128,
        "64 concurrent INSERTs took {elapsed:?} — group-commit not batching?"
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
