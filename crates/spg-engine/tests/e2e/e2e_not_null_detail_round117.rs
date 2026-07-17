//! v7.39 (read01 round 117) — NOT NULL violations carry PG's
//! `DETAIL: Failing row contains (...)`, checked pre-write.
//!
//! SPG's 23502 message omitted the DETAIL line PG attaches, and the two
//! not-null paths disagreed: an omitted no-default column was caught in
//! `build_tuple_pos` before the row existed (no DETAIL), while an explicit NULL
//! was caught deep in storage mid-write (no DETAIL, and after earlier rows of a
//! multi-row INSERT had already been written). Both now go through a single
//! pre-write `enforce_not_null` pass over the fully-assembled rows (INSERT and
//! UPDATE), so the whole statement aborts before any write and the error names
//! the failing row exactly as PG does. Locked byte-identical against PG 18.4.

use spg_engine::{Engine, QueryResult};

fn err(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql) {
        Err(x) => format!("{x}"),
        Ok(ok) => panic!("{sql}: expected error, got {ok:?}"),
    }
}

fn scalar_i64(e: &mut Engine, sql: &str) -> i64 {
    match e.execute(sql).unwrap_or_else(|x| panic!("{sql}: {x:?}")) {
        QueryResult::Rows { rows, .. } => match &rows[0].values[0] {
            spg_storage::Value::BigInt(n) => *n,
            spg_storage::Value::Int(n) => i64::from(*n),
            other => panic!("{sql}: {other:?}"),
        },
        other => panic!("{sql}: {other:?}"),
    }
}

fn setup(e: &mut Engine) {
    e.execute("CREATE TABLE nn (a text, b int NOT NULL, c text DEFAULT 'x')")
        .unwrap();
}

#[test]
fn insert_omitted_not_null_has_detail() {
    let mut e = Engine::new();
    setup(&mut e);
    // Omitted no-default column b lands as NULL; the default column c shows 'x'.
    let m = err(&mut e, "INSERT INTO nn (a) VALUES ('hi')");
    assert!(
        m.contains("null value in column \"b\" of relation \"nn\" violates not-null constraint")
    );
    assert!(
        m.contains("DETAIL: Failing row contains (hi, null, x)."),
        "got: {m}"
    );
}

#[test]
fn insert_explicit_null_has_detail_with_comma() {
    let mut e = Engine::new();
    setup(&mut e);
    // A comma inside a text cell is shown verbatim (PG does not quote it).
    let m = err(&mut e, "INSERT INTO nn VALUES ('p,q', NULL, 'z')");
    assert!(
        m.contains("DETAIL: Failing row contains (p,q, null, z)."),
        "got: {m}"
    );
}

#[test]
fn update_to_null_has_detail() {
    let mut e = Engine::new();
    setup(&mut e);
    e.execute("INSERT INTO nn (a, b) VALUES ('ok', 5)").unwrap();
    let m = err(&mut e, "UPDATE nn SET b = NULL");
    assert!(
        m.contains("null value in column \"b\" of relation \"nn\" violates not-null constraint")
    );
    assert!(
        m.contains("DETAIL: Failing row contains (ok, null, x)."),
        "got: {m}"
    );
}

#[test]
fn multi_row_insert_not_null_is_atomic() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE ta (id int, v int NOT NULL)")
        .unwrap();
    // Row 2 violates NOT NULL; the whole statement must roll back (PG: 0 rows),
    // not leave row 1 behind. The DETAIL names the failing row.
    let m = err(&mut e, "INSERT INTO ta VALUES (1, 10), (2, NULL), (3, 30)");
    assert!(
        m.contains("DETAIL: Failing row contains (2, null)."),
        "got: {m}"
    );
    assert_eq!(scalar_i64(&mut e, "SELECT count(*) FROM ta"), 0);
}
