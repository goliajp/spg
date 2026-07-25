//! read01 round 430 (MySQL differential) — user-defined variables (`@x`).
//!
//! `SET @total = 0; … SELECT @total` is everyday MySQL: migration scripts,
//! running totals, row numbering. SPG had none of it — and worse than
//! missing, it was SILENT on the way in: the parser stripped every `@`, so
//! `@x` and `@@x` were the same node. `SET @x = 5` therefore landed in the
//! session-PARAMETER store under a name nothing reads back and reported
//! success, while `SELECT @x` failed with "Unknown system variable 'x'".
//! A script that only ever wrote never learned anything was wrong.
//!
//! User variables now have their own per-session namespace (beside
//! `last_insert_id` / `row_count`), `:=` is accepted as a second spelling of
//! `=`, an unset variable reads NULL, and `@@` settings are untouched.
//!
//! SCOPE — assignment INSIDE an expression (`SELECT @c := @c + 1 FROM t`,
//! the row-number idiom) is NOT supported: it must mutate session state per
//! row while evaluation holds only `&EvalContext`, and the interior
//! mutability that needs would cost `Engine: Sync`, which the server's
//! `RwLock<Engine>` requires (the same wall documented on `lock_skip_rows`).
//! Statement-level `SET` covers the script and migration usage.
//!
//! Every expectation is copied from a MariaDB 11 run.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn mysql() -> Engine {
    let mut e = Engine::new();
    e.execute("SET sql_mode='STRICT_TRANS_TABLES'").unwrap();
    e
}

fn row(e: &mut Engine, sql: &str) -> Vec<String> {
    match e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err}")) {
        QueryResult::Rows { rows, .. } => rows[0]
            .values
            .iter()
            .map(|v| match v {
                Value::Null => "NULL".to_string(),
                other => spg_engine::eval::value_to_text(other),
            })
            .collect(),
        other => panic!("{sql}: {other:?}"),
    }
}

/// Set then read, for a number and a string.
#[test]
fn set_then_read() {
    let mut e = mysql();
    e.execute("SET @x = 5").unwrap();
    assert_eq!(row(&mut e, "SELECT @x"), vec!["5"]);
    e.execute("SET @s = 'hi'").unwrap();
    assert_eq!(row(&mut e, "SELECT CONCAT(@s, '!')"), vec!["hi!"]);
}

/// An unset variable reads NULL rather than raising.
#[test]
fn unset_reads_null() {
    let mut e = mysql();
    assert_eq!(row(&mut e, "SELECT @never_set"), vec!["NULL"]);
    assert_eq!(row(&mut e, "SELECT @never_set IS NULL"), vec!["true"]);
}

/// `:=` is the same assignment as `=`, and the value may be any expression.
#[test]
fn colon_equals_and_expressions() {
    let mut e = mysql();
    e.execute("SET @e := 2 + 3").unwrap();
    assert_eq!(row(&mut e, "SELECT @e"), vec!["5"]);
    // Reading itself works across statements.
    e.execute("SET @e = @e + 10").unwrap();
    assert_eq!(row(&mut e, "SELECT @e"), vec!["15"]);
}

/// A scalar subquery on the right-hand side.
#[test]
fn subquery_value() {
    let mut e = mysql();
    e.execute("CREATE TABLE t(v INT)").unwrap();
    e.execute("INSERT INTO t VALUES(1),(2),(3)").unwrap();
    e.execute("SET @tot = (SELECT SUM(v) FROM t)").unwrap();
    assert_eq!(row(&mut e, "SELECT @tot"), vec!["6"]);
}

/// Within ONE `SET`, every right-hand side sees the state as it was BEFORE
/// the statement — the assignments do not become visible to each other.
/// Separate statements do chain.
#[test]
fn multi_assignment_is_a_snapshot() {
    let mut e = mysql();
    // Both fresh: @q reads an unset @p, so NULL + 1 is NULL.
    e.execute("SET @p = 1, @q = @p + 1").unwrap();
    assert_eq!(row(&mut e, "SELECT @p, @q"), vec!["1", "NULL"]);
    // @r already 100: @s sees the OLD value, not the new 1.
    e.execute("SET @r = 100").unwrap();
    e.execute("SET @r = 1, @s = @r + 1").unwrap();
    assert_eq!(row(&mut e, "SELECT @r, @s"), vec!["1", "101"]);
    // Separate statements chain normally.
    e.execute("SET @u = 1").unwrap();
    e.execute("SET @w = @u + 1").unwrap();
    assert_eq!(row(&mut e, "SELECT @u, @w"), vec!["1", "2"]);
}

/// `@@` settings keep their own namespace and behaviour — one at-sign and
/// two are unrelated.
#[test]
fn system_variables_untouched() {
    let mut e = mysql();
    assert_eq!(row(&mut e, "SELECT @@autocommit"), vec!["1"]);
    assert_eq!(row(&mut e, "SELECT @@session.autocommit"), vec!["1"]);
    assert_eq!(row(&mut e, "SELECT @@global.autocommit"), vec!["1"]);
    // Setting a USER variable of the same name does not touch the setting.
    e.execute("SET @autocommit = 999").unwrap();
    assert_eq!(row(&mut e, "SELECT @@autocommit"), vec!["1"]);
    assert_eq!(row(&mut e, "SELECT @autocommit"), vec!["999"]);
    // An unknown `@@` name still raises, as MariaDB does.
    assert!(
        e.execute("SELECT @@no_such_setting").is_err(),
        "unknown @@ setting must still raise"
    );
}

/// A PostgreSQL session's `SET`/`current_setting` path is unaffected.
#[test]
fn postgres_set_unchanged() {
    let mut e = Engine::new();
    e.execute("SET search_path TO public").unwrap();
    assert_eq!(
        row(&mut e, "SELECT current_setting('search_path') IS NOT NULL"),
        vec!["true"]
    );
}
