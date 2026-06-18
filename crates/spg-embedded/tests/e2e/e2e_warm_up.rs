//! v7.38 (mailrs prod 7.35 pool-exhaustion incident) — boot-time
//! warm-up API. `Database::warm_up_plan_cache(sqls)` populates the
//! plan-IR cache so the first user-facing request doesn't pay the
//! parse + JOIN-reorder cost; `Database::warm_up_cold_tier()` touches
//! every cold segment so the OS page cache loads them before traffic.

use spg_embedded::Database;

#[test]
fn warm_up_plan_cache_populates_the_engine_plan_cache() {
    let mut db = Database::open_in_memory();
    db.execute("CREATE TABLE m (id BIGSERIAL PRIMARY KEY, sender TEXT, mailbox_id BIGINT)")
        .unwrap();
    db.execute("CREATE TABLE mb (id BIGSERIAL PRIMARY KEY, user_address TEXT)")
        .unwrap();

    // Plan cache starts empty for these SQL strings.
    assert_eq!(db.engine().plan_cache().len(), 0);

    // Pre-warm three shapes the way mailrs would at boot.
    let warmed = db.warm_up_plan_cache(&[
        "SELECT COUNT(*) FROM m JOIN mb ON m.mailbox_id = mb.id WHERE mb.user_address = 'x@x'",
        "SELECT sender FROM m JOIN mb ON m.mailbox_id = mb.id \
         WHERE mb.user_address = 'x@x' GROUP BY sender ORDER BY MAX(id) DESC LIMIT 20",
        "SELECT id FROM m WHERE id = 1",
    ]);
    assert_eq!(warmed, 3);

    // Each SQL is now in the cache; calling prepare_cached again is a
    // pure cache hit (no parser invocation).
    let before = db.engine().plan_cache().len();
    assert!(before >= 3);
    // Same SQL re-prepares cleanly with no growth — confirms the cache
    // is keyed on the exact SQL string and the warm-up populated it.
    let _ = db.engine_mut().prepare_cached(
        "SELECT COUNT(*) FROM m JOIN mb ON m.mailbox_id = mb.id WHERE mb.user_address = 'x@x'",
    );
    assert_eq!(db.engine().plan_cache().len(), before);
}

#[test]
fn warm_up_plan_cache_skips_invalid_sql_silently() {
    let mut db = Database::open_in_memory();
    let warmed = db.warm_up_plan_cache(&[
        "SELECT 1",                         // OK
        "this is not valid SQL at all",     // parse error
        "SELECT * FROM nonexistent_table", // parse OK, planning OK (table check at exec)
    ]);
    // Two SQL strings parse cleanly; the third (1-token gibberish)
    // doesn't. The API counts successes and silently drops failures
    // so a single typo in the mailrs warm-up list doesn't take out
    // the boot path.
    assert_eq!(warmed, 2);
}

#[test]
fn open_path_returns_a_server_ready_catalog_without_client_warmup() {
    // v7.37.2 (zero-customer-change rule per
    // feedback-zero-customer-change-warmup-incident) — the client
    // does not call any `warm_up_*` method. `Database::open_path`
    // is responsible for returning a catalog that's fully ready to
    // serve. This test pins that contract: open + immediate query
    // succeeds and the cold-tier auto-warm path runs without panic
    // even on an empty catalog.
    let tmp = std::env::temp_dir().join(format!(
        "spg-warm-open-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&tmp).unwrap();
    let db_path = tmp.join("auto.spg");
    {
        let mut db = Database::open_path(&db_path).expect("open empty");
        db.execute("CREATE TABLE t (id BIGSERIAL PRIMARY KEY, v TEXT)")
            .unwrap();
        for i in 0..50 {
            db.execute(&format!("INSERT INTO t (v) VALUES ('row{i}')"))
                .unwrap();
        }
    }
    // Re-open. Auto-warm fires inside open_path. Client doesn't see
    // any warm-up call site.
    let mut reopened = Database::open_path(&db_path).expect("reopen");
    let r = reopened.execute("SELECT COUNT(*) FROM t").unwrap();
    if let spg_embedded::QueryResult::Rows { rows, .. } = r {
        if let spg_storage::Value::BigInt(n) = rows[0].values[0] {
            assert_eq!(n, 50);
        }
    }
    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn warm_up_cold_tier_returns_zero_on_hot_only_catalog() {
    let mut db = Database::open_in_memory();
    db.execute("CREATE TABLE t (id BIGSERIAL PRIMARY KEY, v TEXT)")
        .unwrap();
    for i in 0..100 {
        db.execute(&format!("INSERT INTO t (v) VALUES ('row{i}')"))
            .unwrap();
    }
    // No freeze yet — all rows are hot.
    let touched = db.warm_up_cold_tier();
    assert_eq!(touched, 0);
}
