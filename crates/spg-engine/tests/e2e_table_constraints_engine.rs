//! v7.9.19 — engine enforcement of table-level UNIQUE /
//! PRIMARY KEY clauses. mailrs G1 + G6.

use spg_engine::{Engine, EngineError, QueryResult};

fn engine_with(sqls: &[&str]) -> Engine {
    let mut eng = Engine::new();
    for sql in sqls {
        let r = eng
            .execute(sql)
            .unwrap_or_else(|e| panic!("setup {sql:?}: {e:?}"));
        assert!(matches!(r, QueryResult::CommandOk { .. }), "{sql:?}");
    }
    eng
}

fn count(eng: &mut Engine, sql: &str) -> usize {
    match eng.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => rows.len(),
        _ => 0,
    }
}

#[test]
fn composite_unique_rejects_full_duplicate() {
    let mut eng = engine_with(&[
        "CREATE TABLE messages (\
            id BIGSERIAL,\
            mailbox_id BIGINT NOT NULL,\
            uid INTEGER NOT NULL,\
            UNIQUE (mailbox_id, uid)\
        )",
        "INSERT INTO messages (mailbox_id, uid) VALUES (1, 100)",
    ]);
    let r = eng.execute("INSERT INTO messages (mailbox_id, uid) VALUES (1, 100)");
    assert!(matches!(r, Err(EngineError::Unsupported(ref s)) if s.contains("UNIQUE violation")));
    assert_eq!(count(&mut eng, "SELECT id FROM messages"), 1);
}

#[test]
fn composite_unique_allows_partial_match() {
    // Same mailbox_id, different uid → not a duplicate.
    let mut eng = engine_with(&[
        "CREATE TABLE messages (mailbox_id INT NOT NULL, uid INT NOT NULL, UNIQUE (mailbox_id, uid))",
        "INSERT INTO messages VALUES (1, 100)",
    ]);
    eng.execute("INSERT INTO messages VALUES (1, 101)").unwrap();
    assert_eq!(count(&mut eng, "SELECT mailbox_id FROM messages"), 2);
}

#[test]
fn composite_pk_implies_not_null_and_rejects_duplicate() {
    let mut eng = engine_with(&[
        "CREATE TABLE snoozed (\
            thread_id TEXT NOT NULL,\
            account_address TEXT NOT NULL,\
            PRIMARY KEY (thread_id, account_address)\
        )",
        "INSERT INTO snoozed VALUES ('t1', 'a@x.com')",
    ]);
    let r = eng.execute("INSERT INTO snoozed VALUES ('t1', 'a@x.com')");
    assert!(
        matches!(r, Err(EngineError::Unsupported(ref s)) if s.contains("PRIMARY KEY violation"))
    );
}

#[test]
fn composite_unique_within_same_batch_caught() {
    let mut eng = engine_with(&["CREATE TABLE t (a INT NOT NULL, b INT NOT NULL, UNIQUE (a, b))"]);
    let r = eng.execute("INSERT INTO t VALUES (1, 100), (1, 100)");
    assert!(r.is_err());
}

#[test]
fn composite_unique_null_skips_check() {
    // SQL spec: NULL ≠ NULL for uniqueness purposes.
    let mut eng = engine_with(&[
        "CREATE TABLE t (a INT, b INT, UNIQUE (a, b))",
        "INSERT INTO t VALUES (1, NULL)",
    ]);
    eng.execute("INSERT INTO t VALUES (1, NULL)").unwrap();
    assert_eq!(count(&mut eng, "SELECT a FROM t"), 2);
}

#[test]
fn pk_index_is_created_for_table_level_pk() {
    let eng = engine_with(&["CREATE TABLE config (\
            namespace TEXT NOT NULL,\
            cfg_key TEXT NOT NULL,\
            value TEXT NOT NULL,\
            PRIMARY KEY (namespace, cfg_key)\
        )"]);
    let bytes = eng.snapshot();
    let cat = spg_storage::Catalog::deserialize(&bytes).unwrap();
    let table = cat.get("config").unwrap();
    // Leading column has a BTree index.
    assert!(
        table
            .indices()
            .iter()
            .any(|i| matches!(i.kind, spg_storage::IndexKind::BTree(_))),
        "PK leading column index missing"
    );
    // Uniqueness constraint persists across snapshot.
    let ucs = &table.schema().uniqueness_constraints;
    assert_eq!(ucs.len(), 1);
    assert!(ucs[0].is_primary_key);
    assert_eq!(ucs[0].columns.len(), 2);
}

#[test]
fn table_with_pk_and_separate_unique_lands_both() {
    let mut eng = engine_with(&[
        "CREATE TABLE t (\
            a INT NOT NULL,\
            b INT NOT NULL,\
            c INT NOT NULL,\
            PRIMARY KEY (a),\
            UNIQUE (b, c)\
        )",
        "INSERT INTO t VALUES (1, 10, 100)",
    ]);
    // PK collision.
    assert!(eng.execute("INSERT INTO t VALUES (1, 11, 101)").is_err());
    // UNIQUE collision.
    assert!(eng.execute("INSERT INTO t VALUES (2, 10, 100)").is_err());
    // Both clean → ok.
    eng.execute("INSERT INTO t VALUES (3, 30, 300)").unwrap();
    assert_eq!(count(&mut eng, "SELECT a FROM t"), 2);
}

#[test]
fn uniqueness_constraints_round_trip_snapshot() {
    let eng = engine_with(&["CREATE TABLE t (a INT NOT NULL, b INT NOT NULL, UNIQUE (a, b))"]);
    let bytes = eng.snapshot();
    let cat = spg_storage::Catalog::deserialize(&bytes).unwrap();
    let ucs = &cat.get("t").unwrap().schema().uniqueness_constraints;
    assert_eq!(ucs.len(), 1);
    assert!(!ucs[0].is_primary_key);
    assert_eq!(ucs[0].columns, vec![0, 1]);
}
