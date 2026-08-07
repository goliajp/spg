//! read01 round 433 (MySQL differential) — zero and explicit values in an
//! AUTO_INCREMENT column.
//!
//! Two silent divergences, both measured on MariaDB 11.
//!
//! **1. An explicit `0` means "assign one".** MySQL treats `INSERT INTO
//! t(id) VALUES (0)` exactly like `VALUES (NULL)`: it stores the next
//! generated id and LAST_INSERT_ID() reports it. Legacy code and several
//! ORMs write 0 for that, and SPG stored the literal 0 — so the table held
//! a row with id 0 AND every later id sat one short of MySQL's, silently.
//!
//! **2. An explicit value raises the counter for the rest of the
//! statement.** `VALUES (NULL,·),(7,·),(NULL,·)` yields 1, 7, 8 on MariaDB.
//! SPG derives the next id from the table's current max, which has not
//! moved mid-statement, so the third row used to land on 2. This one was
//! independent of the zero rule — it bit plain `NULL` inserts too.
//!
//! A LOWER explicit value never pulls the counter back:
//! `(50,·),(3,·),(NULL,·)` yields 50, 3, 51.
//!
//! Scope, measured and deliberately left alone: `UPDATE … SET id = 0`
//! stores 0 (generation is an INSERT-only rule); a 0 in a NON-auto column
//! stays 0; and PG sessions keep storing the literal 0. MySQL's
//! `NO_AUTO_VALUE_ON_ZERO` sql_mode, which disables rule 1, is not honoured
//! — SPG tracks only the escaping bit of sql_mode today.
//!
//! Every expectation is copied from a MariaDB 11 run.

use spg_engine::{Engine, QueryResult};

fn mysql() -> Engine {
    let mut e = Engine::new();
    e.execute("SET sql_mode='STRICT_TRANS_TABLES'").unwrap();
    e
}

fn rows(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err}")) {
        QueryResult::Rows { rows, .. } => rows
            .iter()
            .map(|r| {
                r.values
                    .iter()
                    .map(spg_engine::eval::value_to_text)
                    .collect::<Vec<_>>()
                    .join("/")
            })
            .collect::<Vec<_>>()
            .join(" "),
        other => panic!("{sql}: {other:?}"),
    }
}

fn auto_table(e: &mut Engine, name: &str) {
    e.execute(&format!(
        "CREATE TABLE {name}(id INT PRIMARY KEY AUTO_INCREMENT, v INT)"
    ))
    .unwrap();
}

#[test]
fn round433_explicit_zero_generates_like_null() {
    let mut e = mysql();
    auto_table(&mut e, "t");
    e.execute("INSERT INTO t(id,v) VALUES (0,10)").unwrap();
    assert_eq!(rows(&mut e, "SELECT id,v FROM t"), "1/10");
    // The generated row also advances the counter, so a following NULL gets 2.
    e.execute("INSERT INTO t(id,v) VALUES (NULL,20)").unwrap();
    assert_eq!(rows(&mut e, "SELECT id,v FROM t ORDER BY id"), "1/10 2/20");
}

#[test]
fn round433_zero_reports_the_generated_last_insert_id() {
    let mut e = mysql();
    auto_table(&mut e, "b");
    e.execute("INSERT INTO b(id,v) VALUES (0,1)").unwrap();
    assert_eq!(rows(&mut e, "SELECT LAST_INSERT_ID()"), "1");
}

#[test]
fn round433_string_zero_generates_too() {
    let mut e = mysql();
    auto_table(&mut e, "f");
    e.execute("INSERT INTO f(id,v) VALUES ('0',1)").unwrap();
    assert_eq!(rows(&mut e, "SELECT id,v FROM f"), "1/1");
}

#[test]
fn round433_multi_row_mixes_zero_null_and_explicit() {
    let mut e = mysql();
    auto_table(&mut e, "a");
    e.execute("INSERT INTO a(id,v) VALUES (0,1),(NULL,2),(7,3),(0,4)")
        .unwrap();
    assert_eq!(
        rows(&mut e, "SELECT id,v FROM a ORDER BY v"),
        "1/1 2/2 7/3 8/4"
    );
}

#[test]
fn round433_explicit_value_raises_the_in_statement_counter() {
    // Independent of the zero rule: plain NULLs drift the same way.
    let mut e = mysql();
    auto_table(&mut e, "m");
    e.execute("INSERT INTO m(id,v) VALUES (NULL,1),(7,2),(NULL,3)")
        .unwrap();
    assert_eq!(rows(&mut e, "SELECT id,v FROM m ORDER BY v"), "1/1 7/2 8/3");
}

#[test]
fn round433_lower_explicit_value_does_not_pull_the_counter_back() {
    let mut e = mysql();
    auto_table(&mut e, "n");
    e.execute("INSERT INTO n(id,v) VALUES (50,1),(3,2),(NULL,3)")
        .unwrap();
    assert_eq!(
        rows(&mut e, "SELECT id,v FROM n ORDER BY v"),
        "50/1 3/2 51/3"
    );
}

#[test]
fn round433_update_to_zero_stores_zero() {
    // Generation is an INSERT-only rule — measured.
    let mut e = mysql();
    auto_table(&mut e, "a");
    e.execute("INSERT INTO a(id,v) VALUES (0,1),(NULL,2),(7,3),(0,4)")
        .unwrap();
    e.execute("UPDATE a SET id=0 WHERE v=3").unwrap();
    assert_eq!(
        rows(&mut e, "SELECT id,v FROM a ORDER BY v"),
        "1/1 2/2 0/3 8/4"
    );
}

#[test]
fn round433_zero_in_a_non_auto_column_stays_zero() {
    let mut e = mysql();
    e.execute("CREATE TABLE d(id INT PRIMARY KEY, v INT)")
        .unwrap();
    e.execute("INSERT INTO d VALUES (0,1)").unwrap();
    assert_eq!(rows(&mut e, "SELECT id,v FROM d"), "0/1");
}

#[test]
fn round433_pg_dialect_still_stores_the_literal_zero() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE g(id BIGSERIAL PRIMARY KEY, v INT)")
        .unwrap();
    e.execute("INSERT INTO g(id,v) VALUES (0,1)").unwrap();
    e.execute("INSERT INTO g(v) VALUES (2)").unwrap();
    assert_eq!(rows(&mut e, "SELECT id,v FROM g ORDER BY v"), "0/1 1/2");
}
