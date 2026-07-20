//! v7.39 (round 277) — SQL-level PREPARE / EXECUTE / DEALLOCATE.
//!
//! All three used to be accepted and dropped on the reasoning that
//! "real execution still happens via the extended-query flow" — true
//! only for a driver that uses that flow. A plain SQL `PREPARE p AS …`
//! followed by `EXECUTE p(…)` both answered success and returned no
//! rows at all.
//!
//! Every expectation was read off live PG 18.4 in a single session
//! (prepared statements are session-scoped, so the usual
//! one-psql-per-statement harness could not see them).

use spg_engine::{Engine, QueryResult};

fn lines(e: &mut Engine, sql: &str) -> Vec<String> {
    let r = e
        .execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    let QueryResult::Rows { rows, .. } = r else {
        panic!("expected Rows from {sql}");
    };
    rows.into_iter()
        .map(|row| {
            row.values
                .iter()
                .map(|v| match v {
                    spg_storage::Value::Null => String::new(),
                    other => spg_engine::eval::value_to_text(other),
                })
                .collect::<Vec<_>>()
                .join("|")
        })
        .collect()
}

fn err(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql) {
        Ok(v) => panic!("{sql}: expected an error, got {v:?}"),
        Err(x) => format!("{x}").replace("unsupported: ", ""),
    }
}

fn fixture() -> Engine {
    let mut e = Engine::new();
    e.execute("CREATE TABLE pt (id int, name text)").unwrap();
    e.execute("INSERT INTO pt VALUES (1,'a'),(2,'b'),(3,'c')")
        .unwrap();
    e
}

#[test]
fn a_prepared_statement_executes_and_returns_rows() {
    let mut e = fixture();
    e.execute("PREPARE p1 AS SELECT id, name FROM pt WHERE id = $1")
        .unwrap();
    // This is the whole point: before this round both statements
    // reported success and EXECUTE produced nothing.
    assert_eq!(lines(&mut e, "EXECUTE p1(2)"), vec!["2|b"]);
    assert_eq!(lines(&mut e, "EXECUTE p1(3)"), vec!["3|c"]);
    // The plan is reusable — a second execute with a different value
    // does not disturb the stored body.
    assert_eq!(lines(&mut e, "EXECUTE p1(1)"), vec!["1|a"]);
}

#[test]
fn declared_parameter_types_and_several_placeholders() {
    let mut e = fixture();
    e.execute(
        "PREPARE p2 (int, text) AS SELECT count(*) FROM pt WHERE id > $1 AND name <> $2",
    )
    .unwrap();
    assert_eq!(lines(&mut e, "EXECUTE p2(1, 'zz')"), vec!["2"]);
}

#[test]
fn the_error_wordings_are_pgs() {
    let mut e = fixture();
    e.execute("PREPARE p1 AS SELECT 1").unwrap();
    assert_eq!(
        err(&mut e, "PREPARE p1 AS SELECT 2"),
        "prepared statement \"p1\" already exists",
    );
    assert_eq!(
        err(&mut e, "EXECUTE nosuch(1)"),
        "prepared statement \"nosuch\" does not exist",
    );
    assert_eq!(
        err(&mut e, "DEALLOCATE nosuch"),
        "prepared statement \"nosuch\" does not exist",
    );
}

#[test]
fn deallocate_removes_one_and_all_removes_every() {
    let mut e = fixture();
    e.execute("PREPARE p1 AS SELECT 1").unwrap();
    e.execute("PREPARE p2 AS SELECT 2").unwrap();
    e.execute("DEALLOCATE p1").unwrap();
    assert_eq!(
        err(&mut e, "EXECUTE p1"),
        "prepared statement \"p1\" does not exist",
    );
    assert_eq!(lines(&mut e, "SELECT count(*) FROM pg_prepared_statements"), vec!["1"]);
    e.execute("DEALLOCATE ALL").unwrap();
    assert_eq!(lines(&mut e, "SELECT count(*) FROM pg_prepared_statements"), vec!["0"]);
}

#[test]
fn pg_prepared_statements_reports_the_session() {
    let mut e = fixture();
    e.execute("PREPARE p1 AS SELECT id FROM pt WHERE id = $1")
        .unwrap();
    e.execute(
        "PREPARE p2 (int, text) AS SELECT count(*) FROM pt WHERE id > $1 AND name <> $2",
    )
    .unwrap();
    // PG normalises the declared names, so `int` reports as `integer`.
    // p1 declared none: PG INFERS {integer} there and SPG reports {} —
    // inference needs real parameter typing, recorded as a residual.
    assert_eq!(
        lines(
            &mut e,
            "SELECT name, parameter_types FROM pg_prepared_statements ORDER BY name",
        ),
        vec!["p1|{}", "p2|{integer,text}"],
    );
}

#[test]
fn a_prepared_write_runs() {
    let mut e = fixture();
    e.execute("PREPARE ins AS INSERT INTO pt VALUES ($1, $2)")
        .unwrap();
    e.execute("EXECUTE ins(4, 'd')").unwrap();
    assert_eq!(lines(&mut e, "SELECT name FROM pt WHERE id = 4"), vec!["d"]);
}
