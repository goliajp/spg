//! v7.37.16 — subquery-semantics PG18 differential corpus (13th sweep).
//!
//! Every assertion below is the live PostgreSQL 18.4 answer captured on the
//! mini bench container (`psql -tA`, NULL rendered `<NULL>` via
//! `coalesce(...::text,'<NULL>')`, error cases run to observe whether PG
//! raises vs returns). This sweep targets the expression/predicate-position
//! subquery surface that prior sweeps had not directly covered:
//!
//!   * scalar-subquery cardinality (>1 row → error, 0 rows → NULL) in the
//!     SELECT list, in WHERE, and inside a larger expression,
//!   * correlated scalar subqueries (count / avg / bare column, empty → NULL,
//!     >1 row → error, 2-level nesting),
//!   * EXISTS / NOT EXISTS (SELECT NULL, SELECT *, empty inner),
//!   * IN / NOT IN with a NULL in the list (the classic 3VL NOT-IN trap),
//!   * `op ANY / ALL / SOME (subquery)` including the empty-subquery corners.
//!
//! SPG matched PG18 on every case **except one**, now fixed: a NULL LHS
//! against an empty ANY/ALL set (`NULL op ALL(empty)` must be TRUE,
//! `NULL op ANY(empty)` must be FALSE — emptiness decides, the NULL is
//! never compared). SPG previously returned NULL, excluding the row. See
//! `null_lhs_empty_any_all_pg_semantics` below; fix in
//! `eval.rs` AnyAll branch (short-circuit empty before seeding saw_null).

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

/// Render one scalar cell the way `psql -tA` prints it (NULL → `<NULL>`).
fn cell(v: &Value) -> String {
    match v {
        Value::Null => "<NULL>".to_string(),
        Value::Bool(b) => if *b { "t" } else { "f" }.to_string(),
        Value::SmallInt(n) => n.to_string(),
        Value::Int(n) => n.to_string(),
        Value::BigInt(n) => n.to_string(),
        Value::Float(f) => f.to_string(),
        Value::Text(s) => s.to_string(),
        other => format!("{other:?}"),
    }
}

/// Single-row single-column scalar result.
fn s1(e: &mut Engine, sql: &str) -> String {
    let r = e
        .execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    let QueryResult::Rows { rows, .. } = r else {
        panic!("{sql}: expected Rows");
    };
    assert_eq!(rows.len(), 1, "{sql}: expected exactly one row");
    cell(&rows[0].values[0])
}

/// Every row rendered as pipe-joined columns, rows joined by comma —
/// mirrors the PG `-tA` capture format used to record ground truth.
fn grid(e: &mut Engine, sql: &str) -> String {
    let r = e
        .execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    let QueryResult::Rows { rows, .. } = r else {
        panic!("{sql}: expected Rows");
    };
    rows.iter()
        .map(|row| row.values.iter().map(cell).collect::<Vec<_>>().join("|"))
        .collect::<Vec<_>>()
        .join(",")
}

/// Assert the statement is rejected (PG raises an error).
fn err(e: &mut Engine, sql: &str) {
    assert!(
        e.execute(sql).is_err(),
        "{sql}: expected an error (PG18 raises here), got Ok"
    );
}

fn setup(e: &mut Engine) {
    e.execute("CREATE TABLE a (id int, k int, x int)").unwrap();
    e.execute("INSERT INTO a VALUES (1,10,100),(2,20,200),(3,20,NULL),(4,30,400)")
        .unwrap();
    e.execute("CREATE TABLE b (id int, k int, y int)").unwrap();
    e.execute("INSERT INTO b VALUES (1,10,5),(2,10,7),(3,20,NULL),(4,40,9)")
        .unwrap();
    e.execute("CREATE TABLE one (id int, v int)").unwrap();
    e.execute("INSERT INTO one VALUES (1,42)").unwrap();
    e.execute("CREATE TABLE emp (id int, k int)").unwrap();
    e.execute("CREATE TABLE nn (v int)").unwrap();
    e.execute("INSERT INTO nn VALUES (1),(2),(NULL)").unwrap();
    e.execute("CREATE TABLE nomatch (v int)").unwrap();
    e.execute("INSERT INTO nomatch VALUES (5),(6)").unwrap();
}

// ---- scalar subquery cardinality -----------------------------------

#[test]
fn scalar_subquery_cardinality() {
    let mut e = Engine::new();
    setup(&mut e);
    // WHERE id=1 → exactly one row → that value.
    assert_eq!(s1(&mut e, "SELECT (SELECT x FROM a WHERE id=1)"), "100");
    // 0 rows → NULL (not an error).
    assert_eq!(s1(&mut e, "SELECT (SELECT x FROM a WHERE false)"), "<NULL>");
    // >1 row → PG raises "more than one row returned by a subquery ...".
    err(&mut e, "SELECT (SELECT x FROM a)");
    // ORDER BY … LIMIT 1 collapses to one row deterministically.
    assert_eq!(
        s1(&mut e, "SELECT (SELECT x FROM a ORDER BY id LIMIT 1)"),
        "100"
    );
}

#[test]
fn scalar_subquery_in_where_and_expr() {
    let mut e = Engine::new();
    setup(&mut e);
    // Scalar subquery on the RHS of a WHERE comparison.
    assert_eq!(
        s1(
            &mut e,
            "SELECT count(*) FROM a WHERE x = (SELECT x FROM a WHERE id=1)"
        ),
        "1"
    );
    // Scalar subquery embedded inside an arithmetic expression.
    assert_eq!(s1(&mut e, "SELECT ((SELECT x FROM a WHERE id=1)+1)"), "101");
    // >1 row still errors when the scalar sits in WHERE.
    err(&mut e, "SELECT count(*) FROM a WHERE x = (SELECT x FROM a)");
}

// ---- correlated scalar subqueries ----------------------------------

#[test]
fn correlated_scalar_subquery() {
    let mut e = Engine::new();
    setup(&mut e);
    // count per correlated key; k=30 has no match in b → 0 (COUNT over empty).
    assert_eq!(
        grid(
            &mut e,
            "SELECT id, (SELECT count(*) FROM b WHERE b.k=a.k) FROM a ORDER BY id"
        ),
        "1|2,2|1,3|1,4|0"
    );
    // correlated avg in WHERE: only id=1 (x=100 > avg(5,7)=6).
    assert_eq!(
        grid(
            &mut e,
            "SELECT id FROM a WHERE x > (SELECT avg(y) FROM b WHERE b.k=a.k) ORDER BY id"
        ),
        "1"
    );
    // correlated aggregate over an empty partition → NULL (max/no COUNT).
    assert_eq!(
        grid(
            &mut e,
            "SELECT id, (SELECT max(y) FROM b WHERE b.k=a.k AND false) FROM a ORDER BY id"
        ),
        "1|<NULL>,2|<NULL>,3|<NULL>,4|<NULL>"
    );
    // correlated bare column returning 0 rows → NULL.
    assert_eq!(
        grid(
            &mut e,
            "SELECT id, (SELECT y FROM b WHERE b.k=a.k AND b.id=99) FROM a ORDER BY id"
        ),
        "1|<NULL>,2|<NULL>,3|<NULL>,4|<NULL>"
    );
    // correlated bare column returning >1 row (k=10 matches two b rows) → error.
    err(
        &mut e,
        "SELECT id, (SELECT y FROM b WHERE b.k=a.k) FROM a ORDER BY id",
    );
}

#[test]
fn nested_two_level_correlation() {
    let mut e = Engine::new();
    setup(&mut e);
    // a.k=10 → b rows y∈{5,7}; one(v=42) > y holds → id 1 kept. Others drop.
    assert_eq!(
        grid(
            &mut e,
            "SELECT id FROM a WHERE EXISTS (SELECT 1 FROM b WHERE b.k=a.k AND \
             EXISTS (SELECT 1 FROM one WHERE one.v > b.y)) ORDER BY id"
        ),
        "1"
    );
}

// ---- EXISTS / NOT EXISTS -------------------------------------------

#[test]
fn exists_and_not_exists() {
    let mut e = Engine::new();
    setup(&mut e);
    assert_eq!(
        grid(
            &mut e,
            "SELECT id FROM a WHERE EXISTS (SELECT 1 FROM b WHERE b.k=a.k) ORDER BY id"
        ),
        "1,2,3"
    );
    assert_eq!(
        grid(
            &mut e,
            "SELECT id FROM a WHERE NOT EXISTS (SELECT 1 FROM b WHERE b.k=a.k) ORDER BY id"
        ),
        "4"
    );
    // EXISTS only cares about row presence, not projected values.
    assert_eq!(
        grid(
            &mut e,
            "SELECT id FROM a WHERE EXISTS (SELECT NULL FROM b WHERE b.k=a.k) ORDER BY id"
        ),
        "1,2,3"
    );
    assert_eq!(
        grid(
            &mut e,
            "SELECT id FROM a WHERE EXISTS (SELECT * FROM b WHERE b.k=a.k) ORDER BY id"
        ),
        "1,2,3"
    );
    // Empty inner → EXISTS false for every outer row.
    assert_eq!(
        grid(
            &mut e,
            "SELECT id FROM a WHERE EXISTS (SELECT 1 FROM b WHERE false) ORDER BY id"
        ),
        ""
    );
}

// ---- IN / NOT IN with NULL (3VL traps) -----------------------------

#[test]
fn in_and_not_in_null_traps() {
    let mut e = Engine::new();
    setup(&mut e);
    // plain IN over a subquery.
    assert_eq!(
        grid(
            &mut e,
            "SELECT id FROM a WHERE k IN (SELECT k FROM b) ORDER BY id"
        ),
        "1,2,3"
    );
    // NOT IN where the inner set contains a NULL and no value matches →
    // result is NULL for every row → all excluded (the classic trap).
    assert_eq!(
        grid(
            &mut e,
            "SELECT v FROM nomatch WHERE v NOT IN (SELECT v FROM nn) ORDER BY v"
        ),
        ""
    );
    // NOT IN over b.y (contains a NULL) → all outer rows excluded.
    assert_eq!(
        grid(
            &mut e,
            "SELECT id FROM a WHERE k NOT IN (SELECT y FROM b) ORDER BY id"
        ),
        ""
    );
    // IN over a NULL-bearing set with no match → excluded (NULL, not true).
    assert_eq!(
        grid(
            &mut e,
            "SELECT id FROM a WHERE x IN (SELECT y FROM b) ORDER BY id"
        ),
        ""
    );
    // Empty inner: NOT IN → all kept, IN → none.
    assert_eq!(
        grid(
            &mut e,
            "SELECT id FROM a WHERE k NOT IN (SELECT k FROM emp) ORDER BY id"
        ),
        "1,2,3,4"
    );
    assert_eq!(
        grid(
            &mut e,
            "SELECT id FROM a WHERE k IN (SELECT k FROM emp) ORDER BY id"
        ),
        ""
    );
}

// ---- ANY / ALL / SOME over a subquery ------------------------------

#[test]
fn any_all_over_subquery() {
    let mut e = Engine::new();
    setup(&mut e);
    // = ANY is IN.
    assert_eq!(
        grid(
            &mut e,
            "SELECT id FROM a WHERE k = ANY(SELECT k FROM b) ORDER BY id"
        ),
        "1,2,3"
    );
    // = ALL only holds if every inner value equals k — here none (mixed set).
    assert_eq!(
        grid(
            &mut e,
            "SELECT id FROM a WHERE k = ALL(SELECT k FROM b) ORDER BY id"
        ),
        ""
    );
    // > ALL over the non-null y set {5,7,9}: 100/200/400 all pass, NULL drops.
    assert_eq!(
        grid(
            &mut e,
            "SELECT id FROM a WHERE x > ALL(SELECT y FROM b WHERE y IS NOT NULL) ORDER BY id"
        ),
        "1,2,4"
    );
    // < ANY over y: no x is below any y → empty.
    assert_eq!(
        grid(
            &mut e,
            "SELECT id FROM a WHERE x < ANY(SELECT y FROM b) ORDER BY id"
        ),
        ""
    );
    // > ALL over a NULL-bearing set → NULL for the comparison → all excluded.
    assert_eq!(
        grid(
            &mut e,
            "SELECT id FROM a WHERE x > ALL(SELECT y FROM b) ORDER BY id"
        ),
        ""
    );
    // <> ALL is NOT IN → NULL in set → all excluded.
    assert_eq!(
        grid(
            &mut e,
            "SELECT id FROM a WHERE k <> ALL(SELECT y FROM b) ORDER BY id"
        ),
        ""
    );
}

#[test]
fn any_all_over_empty_subquery() {
    let mut e = Engine::new();
    setup(&mut e);
    // = ALL(empty) is vacuously true → every row kept.
    assert_eq!(
        grid(
            &mut e,
            "SELECT id FROM a WHERE k = ALL(SELECT k FROM emp) ORDER BY id"
        ),
        "1,2,3,4"
    );
    // = ANY(empty) is false → none.
    assert_eq!(
        grid(
            &mut e,
            "SELECT id FROM a WHERE k = ANY(SELECT k FROM emp) ORDER BY id"
        ),
        ""
    );
    // > ALL(empty) is true even for the NULL-x row (id=3). This was the
    // one divergence found in this sweep — SPG used to drop id=3.
    assert_eq!(
        grid(
            &mut e,
            "SELECT id FROM a WHERE x > ALL(SELECT k FROM emp) ORDER BY id"
        ),
        "1,2,3,4"
    );
}

/// The corrected divergence, at scalar granularity: a NULL LHS against an
/// empty ANY/ALL set is decided purely by emptiness, never by the NULL.
/// PG18: `NULL op ALL(empty)` → t, `NULL op ANY(empty)` → f.
#[test]
fn null_lhs_empty_any_all_pg_semantics() {
    let mut e = Engine::new();
    setup(&mut e);
    assert_eq!(
        s1(&mut e, "SELECT (NULL::int > ALL(SELECT k FROM emp))"),
        "t"
    );
    assert_eq!(
        s1(&mut e, "SELECT (NULL::int = ALL(SELECT k FROM emp))"),
        "t"
    );
    assert_eq!(
        s1(&mut e, "SELECT (NULL::int <> ALL(SELECT k FROM emp))"),
        "t"
    );
    assert_eq!(
        s1(&mut e, "SELECT (NULL::int = ANY(SELECT k FROM emp))"),
        "f"
    );
    assert_eq!(
        s1(&mut e, "SELECT (NULL::int > ANY(SELECT k FROM emp))"),
        "f"
    );
    // Non-null LHS over empty already matched PG; pin it too.
    assert_eq!(s1(&mut e, "SELECT (5 > ALL(SELECT k FROM emp))"), "t");
    assert_eq!(s1(&mut e, "SELECT (5 = ANY(SELECT k FROM emp))"), "f");
    // Non-empty NULL LHS stays NULL (must not be broken by the empty fix).
    assert_eq!(
        s1(&mut e, "SELECT (NULL::int = ANY(SELECT k FROM b))"),
        "<NULL>"
    );
    assert_eq!(
        s1(&mut e, "SELECT (NULL::int = ALL(SELECT k FROM b))"),
        "<NULL>"
    );
}

// ---- scalar aggregate subqueries -----------------------------------

#[test]
fn scalar_aggregate_subquery() {
    let mut e = Engine::new();
    setup(&mut e);
    assert_eq!(s1(&mut e, "SELECT (SELECT count(*) FROM a)"), "4");
    // COUNT over empty → 0, other aggregates over empty → NULL.
    assert_eq!(s1(&mut e, "SELECT (SELECT count(*) FROM emp)"), "0");
    assert_eq!(s1(&mut e, "SELECT (SELECT sum(k) FROM emp)"), "<NULL>");
}
