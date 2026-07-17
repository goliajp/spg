//! v7.39 (read01 round 187, U10) — `ALTER COLUMN … DROP EXPRESSION
//! [IF EXISTS]` matches PG's live-verified semantics (2026-07-18):
//!   * plain form on a non-generated column:
//!     ERROR: column "id" of relation "t" is not a generated column
//!   * IF EXISTS form: NOTICE + skip, statement succeeds
//!     (pg_dump restore scripts rely on the success).
//! Pre-r187 the parser consumed IF EXISTS but dropped it, so both
//! forms errored (with an SPG-flavored message).

use spg_engine::{Engine, QueryResult};

#[test]
fn if_exists_skips_on_plain_column() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT)").unwrap();
    let r = e
        .execute("ALTER TABLE t ALTER COLUMN id DROP EXPRESSION IF EXISTS")
        .unwrap();
    assert!(matches!(r, QueryResult::CommandOk { .. }));
}

#[test]
fn plain_form_errors_with_pg_wording() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT)").unwrap();
    let err = e
        .execute("ALTER TABLE t ALTER COLUMN id DROP EXPRESSION")
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("column \"id\" of relation \"t\" is not a generated column"),
        "unexpected: {err}"
    );
}

#[test]
fn real_generated_column_still_drops() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (a INT, b INT GENERATED ALWAYS AS (a * 2) STORED)")
        .unwrap();
    e.execute("INSERT INTO t (a) VALUES (3)").unwrap();
    e.execute("ALTER TABLE t ALTER COLUMN b DROP EXPRESSION IF EXISTS")
        .unwrap();
    // De-generated: an explicit value is now accepted.
    e.execute("INSERT INTO t VALUES (4, 99)").unwrap();
    match e.execute("SELECT b FROM t ORDER BY a").unwrap() {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows.len(), 2);
        }
        other => panic!("{other:?}"),
    }
}
