//! read01 round 431 (MySQL differential) — `ALTER TABLE … ADD/DROP INDEX`.
//!
//! Every ORM migration tool emits index changes this way. SPG had none of
//! the grammar: all eight MySQL spellings were a parse error, so a MySQL
//! schema migration stopped dead at its first index step.
//!
//! The same grammar already existed for the CREATE TABLE inline form
//! (`KEY idx (a)`, prefix lengths and all), so the ALTER form goes through
//! the SAME parser routine rather than a second copy that could drift.
//!
//! Measured on MariaDB 11:
//!   * `ADD INDEX name (col)` / `ADD KEY` / `ADD UNIQUE INDEX` all work
//!   * `ADD INDEX (col)` unnamed takes the column's name, `_2` on collision
//!   * multi-column and prefix `(c(5))` declarations parse
//!   * a second index on an already-indexed column IS created
//!   * duplicate index name → 1061; unknown column → 1072
//!   * `DROP INDEX name` / `DROP KEY name`; missing → 1091, IF EXISTS ok
//!
//! Every expectation is copied from a MariaDB 11 run.

use spg_engine::{Engine, QueryResult};

fn mysql() -> Engine {
    let mut e = Engine::new();
    e.execute("SET sql_mode='STRICT_TRANS_TABLES'").unwrap();
    e.execute("CREATE TABLE t(a INT, b INT, c VARCHAR(20), d INT)")
        .unwrap();
    e
}

fn ok(e: &mut Engine, sql: &str) {
    match e.execute(sql) {
        Ok(_) => {}
        Err(err) => panic!("{sql}: {err}"),
    }
}

fn err(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql) {
        Ok(r) => panic!("{sql}: expected error, got {r:?}"),
        Err(err) => alloc_string(err),
    }
}

fn alloc_string(err: spg_engine::EngineError) -> String {
    format!("{err}")
}

/// Index names on a table, sorted — read through `pg_indexes`, the same
/// view a client would use.
fn index_names(e: &mut Engine, table: &str) -> Vec<String> {
    let sql =
        format!("SELECT indexname FROM pg_indexes WHERE tablename = '{table}' ORDER BY indexname");
    match e.execute(&sql).unwrap() {
        QueryResult::Rows { rows, .. } => rows
            .iter()
            .map(|r| spg_engine::eval::value_to_text(&r.values[0]))
            .collect(),
        other => panic!("pg_indexes: {other:?}"),
    }
}

#[test]
fn round431_add_index_named_and_synonyms() {
    let mut e = mysql();
    // All four spellings MariaDB accepts.
    ok(&mut e, "ALTER TABLE t ADD INDEX idx_a (a)");
    ok(&mut e, "ALTER TABLE t ADD KEY idx_d (d)");
    ok(&mut e, "ALTER TABLE t ADD UNIQUE INDEX idx_uc (c)");
    ok(&mut e, "ALTER TABLE t ADD INDEX idx_ab (a, b)");

    let names = index_names(&mut e, "t");
    for want in ["idx_a", "idx_d", "idx_ab"] {
        assert!(names.iter().any(|n| n == want), "{want} missing in {names:?}");
    }
}

#[test]
fn round431_add_index_unnamed_takes_column_name() {
    let mut e = mysql();
    // MariaDB: `ADD INDEX (b)` is named `b`; the next one `b_2`, then `b_3`.
    ok(&mut e, "ALTER TABLE t ADD INDEX (b)");
    ok(&mut e, "ALTER TABLE t ADD INDEX (b)");
    ok(&mut e, "ALTER TABLE t ADD INDEX (b)");

    let names = index_names(&mut e, "t");
    for want in ["b", "b_2", "b_3"] {
        assert!(names.iter().any(|n| n == want), "{want} missing in {names:?}");
    }
}

#[test]
fn round431_second_index_on_same_column_is_built() {
    let mut e = mysql();
    ok(&mut e, "ALTER TABLE t ADD INDEX idx_a (a)");
    // MariaDB builds this too — SHOW INDEX lists idx_a AND idx_x on `a`.
    // SPG used to skip it silently, which made the DROP below fail.
    ok(&mut e, "ALTER TABLE t ADD INDEX idx_x (a)");

    let names = index_names(&mut e, "t");
    assert!(names.iter().any(|n| n == "idx_x"), "idx_x missing in {names:?}");
    ok(&mut e, "ALTER TABLE t DROP INDEX idx_x");
}

#[test]
fn round431_prefix_length_parses() {
    let mut e = mysql();
    // MariaDB records sub_part=5; SPG indexes the whole column, which is a
    // superset — the point here is that the declaration is accepted, not
    // rejected as a syntax error.
    ok(&mut e, "ALTER TABLE t ADD INDEX idx_pre (c(5))");
    let names = index_names(&mut e, "t");
    assert!(names.iter().any(|n| n == "idx_pre"), "{names:?}");
}

#[test]
fn round431_multi_action_alter() {
    let mut e = mysql();
    ok(
        &mut e,
        "ALTER TABLE t ADD INDEX idx_x (a), ADD INDEX idx_y (b)",
    );
    let names = index_names(&mut e, "t");
    for want in ["idx_x", "idx_y"] {
        assert!(names.iter().any(|n| n == want), "{want} missing in {names:?}");
    }
}

#[test]
fn round431_duplicate_index_name_is_loud() {
    let mut e = mysql();
    ok(&mut e, "ALTER TABLE t ADD INDEX idx_a (a)");
    // MariaDB: ERROR 1061 (42000) Duplicate key name 'idx_a'.
    let msg = err(&mut e, "ALTER TABLE t ADD INDEX idx_a (b)");
    assert!(
        msg.to_lowercase().contains("idx_a"),
        "expected a duplicate-name error naming idx_a, got {msg}"
    );
}

#[test]
fn round431_unknown_column_is_loud() {
    let mut e = mysql();
    // MariaDB: ERROR 1072 (42000) Key column 'zzz' doesn't exist in table.
    // SPG used to swallow this into a silent no-op.
    let msg = err(&mut e, "ALTER TABLE t ADD INDEX idx_z (zzz)");
    assert!(
        msg.to_lowercase().contains("zzz"),
        "expected an unknown-column error naming zzz, got {msg}"
    );
}

#[test]
fn round431_drop_index_and_key() {
    let mut e = mysql();
    ok(&mut e, "ALTER TABLE t ADD INDEX idx_a (a)");
    ok(&mut e, "ALTER TABLE t ADD KEY idx_d (d)");
    ok(&mut e, "ALTER TABLE t DROP INDEX idx_a");
    ok(&mut e, "ALTER TABLE t DROP KEY idx_d");

    let names = index_names(&mut e, "t");
    for gone in ["idx_a", "idx_d"] {
        assert!(!names.iter().any(|n| n == gone), "{gone} still in {names:?}");
    }
}

#[test]
fn round431_drop_missing_index_is_loud_unless_if_exists() {
    let mut e = mysql();
    // MariaDB: ERROR 1091 (42000) Can't DROP INDEX `nope`.
    let msg = err(&mut e, "ALTER TABLE t DROP INDEX nope");
    assert!(msg.to_lowercase().contains("nope"), "{msg}");
    // MariaDB accepts the IF EXISTS form silently.
    ok(&mut e, "ALTER TABLE t DROP INDEX IF EXISTS nope");
}

#[test]
fn round431_drop_column_named_key_still_works() {
    // `KEY` is only read as the keyword when a name follows it, so a
    // column literally called "key" is still droppable (PG allows it).
    let mut e = Engine::new();
    e.execute("CREATE TABLE k(\"key\" INT, v INT)").unwrap();
    ok(&mut e, "ALTER TABLE k DROP COLUMN \"key\"");
}
