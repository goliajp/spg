//! v7.40.11 — `SHOW` and `EXPLAIN` had no described shape, so over the
//! extended protocol they sent rows a client had no header for.
//!
//! Reported against 7.40.9 (§3.15). psql names it exactly:
//!
//! ```text
//!   server sent data ("D" message) without prior row description ("T" message)
//! ```
//!
//! sqlx surfaces it as a row with zero columns
//! (`ColumnIndexOutOfBounds { index: 0, len: 0 }`), which is how the
//! reporter found it: their timezone readback was `SHOW TimeZone` and
//! could not run. `\gdesc` agreed with the malformed stream rather than
//! with the statement — "the result has no columns".
//!
//! `describe_output_columns` answered a shape for SELECT and for DML
//! RETURNING, and `Vec::new()` for everything else. Execute then emits
//! DataRows for a statement Describe called NoData, which is the
//! violation. Two statement kinds produce rows that way, and both are
//! ordinary: `SHOW` and `EXPLAIN`.
//!
//! Measured on PostgreSQL 18.6 with `\gdesc`:
//!
//! ```text
//!   SHOW TimeZone                     TimeZone      text
//!   SHOW work_mem                     work_mem      text
//!   SHOW ALL                          name/setting/description  text
//!   SHOW TRANSACTION ISOLATION LEVEL  transaction_isolation     text
//!   EXPLAIN SELECT 1                  QUERY PLAN    text
//!   EXPLAIN (FORMAT JSON) SELECT 1    QUERY PLAN    json
//!   EXPLAIN (FORMAT XML) SELECT 1     QUERY PLAN    xml
//!   EXPLAIN (FORMAT YAML) SELECT 1    QUERY PLAN    text
//! ```
//!
//! Two things fall out of the same measurement. The column name is the
//! GUC's CANONICAL spelling, not the one you typed — `SHOW timezone`,
//! `SHOW TimeZone` and `SHOW TIMEZONE` all answer a column called
//! `TimeZone` on PG, and SPG lower-cased whatever arrived, so a client
//! selecting that column by name got nothing. And
//! `SHOW TRANSACTION ISOLATION LEVEL` — PG's own spelling, the one
//! every driver's isolation probe sends — was a syntax error here.

use spg_engine::{Engine, QueryResult};
use spg_storage::DataType;

fn shape(eng: &Engine, sql: &str) -> Vec<(String, DataType)> {
    let stmt = eng
        .prepare(sql)
        .unwrap_or_else(|e| panic!("{sql}: parse: {e:?}"));
    let (_, cols) = eng.describe_prepared(&stmt);
    cols.into_iter().map(|c| (c.name, c.ty)).collect()
}

fn ran(eng: &mut Engine, sql: &str) -> Vec<(String, DataType)> {
    match eng.execute(sql).unwrap_or_else(|e| panic!("{sql}: {e:?}")) {
        QueryResult::Rows { columns, .. } => columns.into_iter().map(|c| (c.name, c.ty)).collect(),
        other => panic!("{sql}: expected Rows, got {other:?}"),
    }
}

/// Describe and Execute must agree about the shape, for every statement
/// that produces rows. A Describe that answers nothing is what puts a
/// DataRow on the wire with no RowDescription in front of it.
#[test]
fn describe_and_execute_agree_about_every_row_producing_statement() {
    let mut eng = Engine::new();
    for sql in [
        "SHOW TimeZone",
        "SHOW work_mem",
        "SHOW ALL",
        "SHOW TRANSACTION ISOLATION LEVEL",
        "EXPLAIN SELECT 1",
        "EXPLAIN (FORMAT JSON) SELECT 1",
        "EXPLAIN (FORMAT XML) SELECT 1",
        "EXPLAIN (FORMAT YAML) SELECT 1",
        "SELECT 1",
    ] {
        let described = shape(&eng, sql);
        assert!(!described.is_empty(), "{sql}: Describe answered no columns");
        let executed = ran(&mut eng, sql);
        assert_eq!(described, executed, "{sql}: Describe vs Execute");
    }
}

/// The GUC's canonical spelling, whichever spelling was typed. PG
/// answers a column called `TimeZone` for all three of these.
#[test]
fn a_shows_column_is_the_gucs_canonical_name() {
    let mut eng = Engine::new();
    for sql in ["SHOW TimeZone", "SHOW timezone", "SHOW TIMEZONE"] {
        assert_eq!(
            ran(&mut eng, sql)[0].0,
            "TimeZone",
            "{sql}: the canonical name, not the one typed"
        );
        assert_eq!(shape(&eng, sql)[0].0, "TimeZone", "{sql}: and at Describe");
    }
    // One that is canonically lower-case, so the rule is "canonical",
    // not "capitalise".
    assert_eq!(ran(&mut eng, "SHOW WORK_MEM")[0].0, "work_mem");
    assert_eq!(ran(&mut eng, "SHOW search_path")[0].0, "search_path");
}

/// PG's own spelling of the isolation probe, which every driver sends.
#[test]
fn show_transaction_isolation_level_parses() {
    let mut eng = Engine::new();
    let cols = ran(&mut eng, "SHOW TRANSACTION ISOLATION LEVEL");
    assert_eq!(cols.len(), 1);
    assert_eq!(cols[0].0, "transaction_isolation");
    assert_eq!(cols[0].1, DataType::Text);
    // And it answers the same thing the underscore spelling does.
    let a = match eng.execute("SHOW TRANSACTION ISOLATION LEVEL").unwrap() {
        QueryResult::Rows { rows, .. } => rows[0].values[0].clone(),
        other => panic!("{other:?}"),
    };
    let b = match eng.execute("SHOW transaction_isolation").unwrap() {
        QueryResult::Rows { rows, .. } => rows[0].values[0].clone(),
        other => panic!("{other:?}"),
    };
    assert_eq!(a, b, "two spellings, one parameter");
}

/// `EXPLAIN`'s column carries the format's type, which is how a client
/// knows to parse the payload rather than print it.
#[test]
fn explains_column_type_follows_the_format() {
    let eng = Engine::new();
    for (sql, want) in [
        ("EXPLAIN SELECT 1", DataType::Text),
        ("EXPLAIN (FORMAT TEXT) SELECT 1", DataType::Text),
        ("EXPLAIN (FORMAT JSON) SELECT 1", DataType::Json),
        ("EXPLAIN (FORMAT XML) SELECT 1", DataType::Xml),
        ("EXPLAIN (FORMAT YAML) SELECT 1", DataType::Text),
    ] {
        let cols = shape(&eng, sql);
        assert_eq!(cols.len(), 1, "{sql}");
        assert_eq!(cols[0].0, "QUERY PLAN", "{sql}");
        assert_eq!(cols[0].1, want, "{sql}");
    }
}

/// `SHOW ALL`'s triple, which PG names name/setting/description.
#[test]
fn show_all_describes_its_three_columns() {
    let eng = Engine::new();
    let cols = shape(&eng, "SHOW ALL");
    assert_eq!(
        cols,
        vec![
            ("name".to_string(), DataType::Text),
            ("setting".to_string(), DataType::Text),
            ("description".to_string(), DataType::Text),
        ]
    );
}
