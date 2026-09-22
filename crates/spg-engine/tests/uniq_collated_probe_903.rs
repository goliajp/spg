//! 9.0.3 — writes on a table with a text key cost time in proportion to
//! the table, under the collation every shipped image runs (`en_US.utf8`).
//!
//! Measured through sentori's ingest transaction against PostgreSQL 18.6,
//! same limits, same client: 164 tps against 1,599, and falling as the
//! tables grew. Three defects, each a whole-table pass per statement:
//!
//! - a text UNIQUE / PRIMARY KEY declined its locale-collated index and
//!   folded every row to check one insert;
//! - ON CONFLICT found its conflict, and the row DO UPDATE rewrites, by
//!   reading every row;
//! - an UPDATE, ON CONFLICT DO UPDATE or MERGE appended its new row
//!   version without the collated index's key, which retired the index,
//!   and the statement's end rebuilt it from every row.
//!
//! The assertions count those passes rather than time them: a timing bound
//! loose enough for a shared machine would not see a factor this large
//! reliably, and the passes ARE the defect. Its own target, run by the gate
//! with `--features perf-counters`, because the counters are process-global
//! (see `uniq_composite_probe.rs`).

use spg_engine::{Engine, QueryResult};
use std::sync::atomic::Ordering::Relaxed;

fn count(e: &mut Engine, sql: &str) -> i64 {
    let QueryResult::Rows { rows, .. } = e.execute(sql).unwrap_or_else(|x| panic!("{sql}: {x:?}"))
    else {
        panic!("{sql}: not rows");
    };
    match rows[0].values[0] {
        spg_storage::Value::BigInt(n) => n,
        ref v => panic!("{sql}: {v:?}"),
    }
}

#[test]
fn text_keyed_writes_do_not_pass_over_the_table() {
    let counted = {
        let before = spg_engine::UNIQ_PROBE_CALLS.load(Relaxed);
        let mut probe = Engine::new();
        probe
            .execute("CREATE TABLE c (id INT PRIMARY KEY)")
            .unwrap();
        probe.execute("INSERT INTO c VALUES (1)").unwrap();
        spg_engine::UNIQ_PROBE_CALLS.load(Relaxed) > before
    };
    if !counted {
        eprintln!(
            "9.0.3 counters gate: perf-counters is OFF, nothing asserted \
             (the gate runner passes --features perf-counters)"
        );
        return;
    }

    let mut e = Engine::new();
    e.set_database_collation("en_US.utf8").unwrap();
    for sql in [
        "CREATE TABLE users (email text PRIMARY KEY, n int NOT NULL DEFAULT 0)",
        "CREATE TABLE hits (issue_id int, user_key text, n bigint NOT NULL DEFAULT 1, \
         PRIMARY KEY (issue_id, user_key))",
        "INSERT INTO users (email) SELECT 'u' || g || '@x' FROM generate_series(1, 2000) g",
        "INSERT INTO hits (issue_id, user_key) SELECT g % 20, 'u' || g FROM generate_series(1, 2000) g",
        // The first write after a bulk load may fill a tree once; the
        // counters below start after it.
        "INSERT INTO users (email) VALUES ('warm@x')",
        "UPDATE users SET n = n + 1 WHERE email = 'warm@x'",
    ] {
        e.execute(sql).unwrap_or_else(|x| panic!("{sql}: {x:?}"));
    }

    let folds = spg_engine::UNIQ_TABLE_FOLDS.load(Relaxed);
    let scans = spg_engine::ON_CONFLICT_ROW_SCANS.load(Relaxed);
    let rebuilds = spg_engine::INDEX_REBUILDS.load(Relaxed);

    e.execute("INSERT INTO users (email) VALUES ('new@x')")
        .unwrap();
    let dup = e.execute("INSERT INTO users (email) VALUES ('u5@x')");
    assert!(
        dup.is_err(),
        "a duplicate text key must still be refused: {dup:?}"
    );
    e.execute("INSERT INTO users (email) VALUES ('u5@x') ON CONFLICT DO NOTHING")
        .unwrap();
    e.execute("INSERT INTO users (email) VALUES ('u5@x') ON CONFLICT (email) DO UPDATE SET n = users.n + 1")
        .unwrap();
    e.execute("INSERT INTO hits VALUES (5, 'u5', 1) ON CONFLICT (issue_id, user_key) DO UPDATE SET n = hits.n + 1 RETURNING n")
        .unwrap();
    e.execute("INSERT INTO hits VALUES (5, 'fresh', 1) ON CONFLICT (issue_id, user_key) DO UPDATE SET n = hits.n + 1")
        .unwrap();
    e.execute("UPDATE users SET n = n + 1 WHERE email = 'u7@x'")
        .unwrap();
    e.execute("UPDATE users SET email = 'renamed@x' WHERE email = 'u8@x'")
        .unwrap();
    e.execute(
        "MERGE INTO users u USING (VALUES ('u9@x'), ('merged@x')) s(e) ON u.email = s.e \
         WHEN MATCHED THEN UPDATE SET n = u.n + 1 WHEN NOT MATCHED THEN INSERT (email, n) VALUES (s.e, 0)",
    )
    .unwrap();

    assert_eq!(
        spg_engine::UNIQ_TABLE_FOLDS.load(Relaxed) - folds,
        0,
        "whole-table uniqueness folds"
    );
    assert_eq!(
        spg_engine::ON_CONFLICT_ROW_SCANS.load(Relaxed) - scans,
        0,
        "ON CONFLICT row scans"
    );
    assert_eq!(
        spg_engine::INDEX_REBUILDS.load(Relaxed) - rebuilds,
        0,
        "whole-index rebuilds"
    );

    // …and the indexes that were kept in service answer correctly.
    assert_eq!(
        count(
            &mut e,
            "SELECT count(*) FROM users WHERE email = 'renamed@x'"
        ),
        1
    );
    assert_eq!(
        count(&mut e, "SELECT count(*) FROM users WHERE email = 'u8@x'"),
        0
    );
    assert_eq!(
        count(
            &mut e,
            "SELECT count(*) FROM users WHERE email = 'merged@x'"
        ),
        1
    );
    assert_eq!(
        count(&mut e, "SELECT n::bigint FROM users WHERE email = 'u5@x'"),
        1
    );
    assert_eq!(
        count(
            &mut e,
            "SELECT n FROM hits WHERE issue_id = 5 AND user_key = 'u5'"
        ),
        2
    );
}
