//! v7.37.7 C.1 — `JOIN … USING (col_list)` parser sugar.
//!
//! Desugars to `prev.col = right.col [AND …]` at parse time;
//! prev = previous join's table (or FROM primary) on the left side.
//! Matches PG semantics for the predicate; column-merge output
//! semantics is a separate v7.38+ item.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn build_engine() -> Engine {
    let mut e = Engine::new();
    e.execute("CREATE TABLE a (id INT NOT NULL, name TEXT NOT NULL)")
        .unwrap();
    e.execute("CREATE TABLE b (id INT NOT NULL, val INT NOT NULL)")
        .unwrap();
    e.execute("INSERT INTO a VALUES (1, 'x'), (2, 'y'), (3, 'z')")
        .unwrap();
    e.execute("INSERT INTO b VALUES (1, 100), (2, 200)")
        .unwrap();
    e
}

fn rowcount(r: &QueryResult) -> usize {
    match r {
        QueryResult::Rows { rows, .. } => rows.len(),
        _ => 0,
    }
}

fn first_int(r: &QueryResult) -> i64 {
    match r {
        QueryResult::Rows { rows, .. } => {
            assert!(!rows.is_empty(), "expected at least one row");
            match &rows[0].values[0] {
                Value::Int(v) => *v as i64,
                Value::BigInt(v) => *v,
                Value::SmallInt(v) => *v as i64,
                other => panic!("expected integer, got {other:?}"),
            }
        }
        _ => panic!("expected Rows"),
    }
}

#[test]
fn join_using_single_column_matches_on_form() {
    let mut e = build_engine();
    let using = e
        .execute("SELECT COUNT(*) FROM a JOIN b USING (id)")
        .expect("USING parses");
    let on = e
        .execute("SELECT COUNT(*) FROM a JOIN b ON a.id = b.id")
        .expect("ON parses");
    assert_eq!(
        first_int(&using),
        first_int(&on),
        "USING (id) must match ON a.id = b.id"
    );
    assert_eq!(first_int(&using), 2);
}

#[test]
fn join_using_with_aliases() {
    let mut e = build_engine();
    let r = e
        .execute("SELECT x.name, y.val FROM a x JOIN b y USING (id)")
        .expect("USING with aliases parses");
    assert_eq!(rowcount(&r), 2);
}

#[test]
fn left_join_using_keeps_orphan_rows() {
    let mut e = build_engine();
    let r = e
        .execute("SELECT a.id FROM a LEFT JOIN b USING (id)")
        .expect("LEFT JOIN USING parses");
    assert_eq!(rowcount(&r), 3, "left join keeps a-row id=3");
}

#[test]
fn join_using_multi_column() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE p (k1 INT NOT NULL, k2 INT NOT NULL, p_val TEXT NOT NULL)")
        .unwrap();
    e.execute("CREATE TABLE q (k1 INT NOT NULL, k2 INT NOT NULL, q_val TEXT NOT NULL)")
        .unwrap();
    e.execute("INSERT INTO p VALUES (1, 10, 'x'), (1, 11, 'y'), (2, 20, 'z')")
        .unwrap();
    e.execute("INSERT INTO q VALUES (1, 10, 'A'), (2, 20, 'B')")
        .unwrap();
    let r = e
        .execute("SELECT COUNT(*) FROM p JOIN q USING (k1, k2)")
        .expect("multi-col USING parses");
    assert_eq!(first_int(&r), 2);
}

#[test]
fn join_using_missing_parens_errors() {
    let mut e = build_engine();
    let err = e
        .execute("SELECT * FROM a JOIN b USING id")
        .expect_err("USING must be followed by '('");
    // v7.39 (round 340, V56) — PG 18.4 measured, verbatim:
    // `syntax error at or near "id"`. It used to be SPG's own prose
    // naming the USING grammar; PG has only its two syntax wordings.
    assert_eq!(format!("{err}"), "parse: syntax error at or near \"id\"");
}

#[test]
fn join_using_empty_paren_errors() {
    let mut e = build_engine();
    let err = e
        .execute("SELECT * FROM a JOIN b USING ()")
        .expect_err("USING () should reject — must list at least one column");
    // PG 18.4: `syntax error at or near ")"`.
    assert_eq!(format!("{err}"), "parse: syntax error at or near \")\"");
}
