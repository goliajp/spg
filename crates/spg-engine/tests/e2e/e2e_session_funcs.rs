//! v7.17.0 Phase 3.P0-30 — session / introspection functions.
//!
//! Reference:
//!   PG: https://www.postgresql.org/docs/current/functions-info.html
//!   MySQL: https://dev.mysql.com/doc/refman/8.0/en/information-functions.html
//!
//! Surface covered (eval level — distinct from pgwire's
//! canned-response shortcuts which only fire on bare `SELECT fn()`):
//!   * `current_database()` (PG) / `database()` (MySQL) → 'spg'
//!   * `current_schema()` → 'public'
//!   * `version()` → 'PostgreSQL 18.4 (SPG-compat)' (already shipped
//!     — corpus-locked here so regression notices)
//!   * `current_user()` / `user()` → 'admin' (paren form)
//!
//! These must work as ordinary expressions — e.g.
//! `WHERE schemaname = current_schema()` or
//! `SELECT t.*, database() AS db FROM t`. The pgwire canned-response
//! path only catches the bare top-level SELECT shape, which leaves
//! every other emission spot in ORM-generated SQL uncovered without
//! this engine arm.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn one_row(r: QueryResult) -> Vec<Value<'static>> {
    match r {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows.len(), 1);
            rows.into_iter().next().unwrap().values
        }
        _ => panic!("expected Rows"),
    }
}

fn one_text(eng: &mut Engine, sql: &str) -> String {
    let row = one_row(eng.execute(sql).unwrap());
    match row.into_iter().next().unwrap() {
        Value::Text(s) => s.into_owned(),
        other => panic!("{sql}: expected Text, got {other:?}"),
    }
}

#[test]
fn current_database_returns_spg() {
    let mut e = Engine::new();
    assert_eq!(one_text(&mut e, "SELECT current_database()"), "spg");
}

#[test]
fn mysql_database_alias_returns_spg() {
    let mut e = Engine::new();
    assert_eq!(one_text(&mut e, "SELECT database()"), "spg");
}

#[test]
fn current_schema_returns_public() {
    let mut e = Engine::new();
    assert_eq!(one_text(&mut e, "SELECT current_schema()"), "public");
}

#[test]
fn version_returns_postgres_compat_banner() {
    let mut e = Engine::new();
    let v = one_text(&mut e, "SELECT version()");
    assert!(
        v.starts_with("PostgreSQL"),
        "version() should start with 'PostgreSQL', got {v:?}"
    );
}

#[test]
fn current_user_paren_returns_admin() {
    let mut e = Engine::new();
    assert_eq!(one_text(&mut e, "SELECT current_user()"), "admin");
}

#[test]
fn database_in_select_list_alongside_table_column() {
    // The pgwire canned-response only catches the bare top-level
    // shape; this verifies the engine arm handles in-expression use.
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT NOT NULL)").unwrap();
    e.execute("INSERT INTO t VALUES (1)").unwrap();
    let row = one_row(
        e.execute("SELECT id, current_database() AS db FROM t")
            .unwrap(),
    );
    assert_eq!(row.len(), 2);
    assert!(matches!(row[0], Value::Int(1)));
    assert!(matches!(&row[1], Value::Text(s) if s == "spg"));
}

/// U21 (read01 A-group): SPG positions as a PG 18 drop-in, so the
/// reported server version must be 18.x — clients gate feature
/// availability on server_version / server_version_num. It used to
/// report 16. Anchored to the live PG 18.4 the dropin panel targets.
#[test]
fn reported_server_version_is_pg18() {
    let mut e = Engine::new();
    let v = one_text(&mut e, "SELECT version()");
    assert!(v.contains("18.4"), "version() should report 18.4, got {v:?}");
    assert_eq!(
        one_text(&mut e, "SELECT current_setting('server_version')"),
        "18.4 (SPG-compat)"
    );
    assert_eq!(
        one_text(&mut e, "SELECT current_setting('server_version_num')"),
        "180004"
    );
}
