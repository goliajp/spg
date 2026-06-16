//! S0 — empirical baseline for WAL-replay cost (root cause A of the
//! crash-recovery P0, `crash-recovery-lock-replay-design-2026-06-16.md`).
//!
//! `mem::forget(db)` skips Drop's checkpoint, leaving N uncheckpointed
//! single-row INSERT records in the WAL (the realistic auto-commit replay
//! shape); reopening times the replay against an M-row base snapshot.
//!
//! Decomposes WHERE replay time goes by varying the base catalog size M:
//!   - time ∝ M  ⟹ superlinear per-statement RE-VALIDATION (each replayed
//!                 INSERT re-checks uniqueness against the whole table) →
//!                 row-level redo (skip re-validation) is the fix;
//!   - time flat in M ⟹ linear per-statement parse/plan/data overhead →
//!                 the fix is bulk-apply, not re-validation removal.
//!
//! The seed phase is BATCHED (few large INSERT records) so its parse cost
//! doesn't confound the measurement; only the N single-row appended
//! records are replayed (snapshot_lsn floor skips the checkpointed seed).
//!
//! `#[ignore]` — run with:
//!   cargo test --release -p spg-embedded --test e2e \
//!     -- --ignored --nocapture replay_cost

use std::path::PathBuf;
use std::time::Instant;

use spg_embedded::Database;

fn unique_tmpdir(label: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let p = std::env::temp_dir().join(format!("spg-replay-probe-{label}-{nanos}"));
    std::fs::create_dir_all(&p).unwrap();
    p
}

/// Seed `base` rows (checkpointed into the snapshot), append `appended`
/// uncheckpointed single-row INSERTs, skip Drop, then time the reopen
/// (= replay of the appended records against the base).
fn replay_time(label: &str, base: usize, appended: usize, fat: bool) {
    let dir = unique_tmpdir(label);
    let db_path = dir.join("spg.db");
    let fat_val = "x".repeat(2000);
    {
        let mut db = Database::open_path(&db_path).unwrap();
        if fat {
            db.execute("CREATE TABLE t (id INT PRIMARY KEY, body TEXT)")
                .unwrap();
        } else {
            db.execute("CREATE TABLE t (id INT PRIMARY KEY)").unwrap();
        }
        // Seed BATCHED (500/record) so seed parse cost is O(base/500),
        // not O(base) — keeps the replay measurement clean.
        let mut i = 0usize;
        while i < base {
            let end = (i + 500).min(base);
            let vals: Vec<String> = (i..end)
                .map(|k| {
                    if fat {
                        format!("({k}, '{fat_val}')")
                    } else {
                        format!("({k})")
                    }
                })
                .collect();
            db.execute(&format!("INSERT INTO t VALUES {}", vals.join(",")))
                .unwrap();
            i = end;
        }
        // Base into the snapshot, WAL rotated past it (floor set).
        db.checkpoint().unwrap();
        // Append N uncheckpointed SINGLE-ROW inserts = realistic
        // per-execute auto-commit WAL records.
        for k in base..(base + appended) {
            let v = if fat {
                format!("({k}, '{fat_val}')")
            } else {
                format!("({k})")
            };
            db.execute(&format!("INSERT INTO t VALUES {v}")).unwrap();
        }
        std::mem::forget(db); // skip Drop checkpoint → N records stay in WAL
    }
    Database::force_unlock(&db_path).unwrap();
    let t = Instant::now();
    let mut db = Database::open_path(&db_path).unwrap();
    let elapsed = t.elapsed();
    // sanity: all rows present after replay
    let got = db.query("SELECT count(*) FROM t").unwrap();
    let n = match &got[0][0] {
        spg_storage::Value::BigInt(n) => *n,
        other => panic!("count: {other:?}"),
    };
    assert_eq!(n as usize, base + appended, "{label}: replay lost rows");
    let per = elapsed / (appended.max(1) as u32);
    eprintln!(
        "{label:10} base={base:>6} append={appended:>5} fat={fat:<5} → replay {elapsed:>9.2?}  ({per:>8.2?}/rec)"
    );
    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
#[ignore = "perf probe; run with --ignored --nocapture"]
fn replay_cost_decomposition() {
    eprintln!("== vary base M at fixed append N=2000 (time ∝ M ⟹ superlinear re-validation) ==");
    for base in [0usize, 5_000, 10_000, 20_000] {
        replay_time("vary-M", base, 2_000, false);
    }
    eprintln!("== vary append N at fixed base M=10000 ==");
    for n in [500usize, 1_000, 2_000, 4_000] {
        replay_time("vary-N", 10_000, n, false);
    }
    eprintln!("== fat rows (2KB body) base=10000 append=2000 ==");
    replay_time("fat", 10_000, 2_000, true);
}

/// v7.34 — quantify the synchronous full-serialize `checkpoint()` cost
/// (the hot-path stall + whole-catalog block that checkpoint CoW /
/// background moves off the foreground). Time ∝ hot-tier size: every
/// checkpoint re-serialises all hot rows while holding the engine, so a
/// big catalog blocks every concurrent query for this long. This is the
/// baseline the CoW optimisation must drive to ~0 on the hot path.
#[test]
#[ignore = "perf probe; run with --ignored --nocapture"]
fn checkpoint_cost_by_size() {
    eprintln!("== checkpoint() blocking cost by hot-tier size ==");
    for base in [0usize, 5_000, 10_000, 20_000] {
        let dir = unique_tmpdir(&format!("ckpt-{base}"));
        let db_path = dir.join("spg.db");
        let mut db = Database::open_path(&db_path).unwrap();
        db.execute("CREATE TABLE t (id INT PRIMARY KEY, body TEXT)")
            .unwrap();
        let mut i = 0usize;
        while i < base {
            let end = (i + 500).min(base);
            let vals: Vec<String> = (i..end)
                .map(|k| format!("({k}, 'row {k} body text padding xxxxxxxx')"))
                .collect();
            db.execute(&format!("INSERT INTO t VALUES {}", vals.join(",")))
                .unwrap();
            i = end;
        }
        let t = Instant::now();
        db.checkpoint().unwrap();
        let elapsed = t.elapsed();
        eprintln!("checkpoint base={base:>6} → {elapsed:>9.2?}  (blocks all queries this long)");
        drop(db);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
