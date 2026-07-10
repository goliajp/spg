//! v7.38 (read01) — the DEFAULT expression's source text is preserved and
//! surfaced by `information_schema.columns.column_default`, `pg_attrdef`, and
//! `pg_get_expr(adbin, adrelid)`. Root fix: `ColumnSchema.default_text` caches
//! the deparsed source at CREATE TABLE, so `numeric(10,2) DEFAULT 0` reports
//! PG's `0` (not the coerced render `0.00`). Every expected value is from live
//! PG18.4.
//!
//! Byte-identical-to-PG shapes are asserted here. Known Phase-2 deparse
//! residuals (wide-int cast, per-operand typing of literals nested inside an
//! expression) are tracked in the read01 checklist, not asserted.

use spg_engine::{Engine, QueryResult};

/// One scalar text (or "NULL") per row, in row order.
fn col(e: &mut Engine, sql: &str) -> Vec<String> {
    match e
        .execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"))
    {
        QueryResult::Rows { rows, .. } => rows
            .iter()
            .map(|r| match &r.values[0] {
                spg_storage::Value::Text(s) => s.to_string(),
                spg_storage::Value::Null => "NULL".to_string(),
                other => format!("{other:?}"),
            })
            .collect(),
        other => panic!("{sql}: expected Rows, got {other:?}"),
    }
}

/// Two columns joined by '|', one string per row.
fn pair(e: &mut Engine, sql: &str) -> Vec<String> {
    match e
        .execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"))
    {
        QueryResult::Rows { rows, .. } => rows
            .iter()
            .map(|r| {
                r.values
                    .iter()
                    .map(|v| match v {
                        spg_storage::Value::Text(s) => s.to_string(),
                        spg_storage::Value::SmallInt(n) => n.to_string(),
                        spg_storage::Value::Null => "NULL".to_string(),
                        other => format!("{other:?}"),
                    })
                    .collect::<Vec<_>>()
                    .join("|")
            })
            .collect(),
        other => panic!("{sql}: expected Rows, got {other:?}"),
    }
}

const DDL: &str = "CREATE TABLE dt(\
    a int DEFAULT 0,\
    b numeric(10,2) DEFAULT 0,\
    n numeric(10,2) DEFAULT 0.5,\
    c text DEFAULT 'hi',\
    v varchar(10) DEFAULT 'x',\
    q text DEFAULT 'a''b',\
    g boolean DEFAULT true,\
    d timestamp DEFAULT now(),\
    h date DEFAULT CURRENT_DATE,\
    e int DEFAULT 3+4,\
    m int DEFAULT -5,\
    p numeric DEFAULT -1.5,\
    f int)";

#[test]
fn column_default_reports_pg_source_text() {
    let mut e = Engine::new();
    e.execute(DDL).unwrap();
    let got = col(
        &mut e,
        "SELECT COALESCE(column_default,'NULL') FROM information_schema.columns \
         WHERE table_name='dt' ORDER BY ordinal_position",
    );
    assert_eq!(
        got,
        vec![
            "0",                      // a int
            "0",                      // b numeric(10,2) DEFAULT 0  (was 0.00)
            "0.5",                    // n numeric(10,2) DEFAULT 0.5
            "'hi'::text",             // c text
            "'x'::character varying", // v varchar(10)
            "'a''b'::text",           // q text, escaped quote
            "true",                   // g boolean
            "now()",                  // d timestamp
            "CURRENT_DATE",           // h date
            "(3 + 4)",                // e int arithmetic
            "'-5'::integer",          // m int negative
            "'-1.5'::numeric",        // p numeric negative
            "NULL",                   // f no default
        ]
    );
}

#[test]
fn pg_attrdef_lists_defaults_with_pg_get_expr() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t(id int DEFAULT 7, v text DEFAULT 'hi', plain int)")
        .unwrap();
    // Only the two defaulted columns appear; pg_get_expr returns the text.
    let got = pair(
        &mut e,
        "SELECT adnum, pg_get_expr(adbin, adrelid) FROM pg_attrdef ORDER BY adnum",
    );
    assert_eq!(got, vec!["1|7", "2|'hi'::text"]);
}

#[test]
fn pg_attrdef_empty_when_no_defaults() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t(a int, b text)").unwrap();
    let got = pair(
        &mut e,
        "SELECT adnum, pg_get_expr(adbin, adrelid) FROM pg_attrdef",
    );
    assert!(got.is_empty(), "expected no pg_attrdef rows, got {got:?}");
}
