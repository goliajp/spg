//! v7.39 (read01 round 80) — a sweep of the set-operation / ordering surface.
//! UNION / UNION ALL / INTERSECT [ALL] / EXCEPT [ALL], NULL identity in each,
//! NULLS FIRST/LAST, COLLATE "C", type unification across branches: all already
//! matched PG. One thing did not.
//!
//!     SELECT unnest(ARRAY['B','a','A','b']) ORDER BY 1;
//!     PG:   A, B, a, b
//!     SPG:  B, a, A, b        -- i.e. input order: the sort did nothing
//!
//! `ORDER BY <n>` names the Nth OUTPUT column. Three executors evaluated the key
//! as an ordinary expression, where the literal `n` is just the constant n — the
//! same sort key for every row. So the sort ran, compared equal every time, and
//! changed nothing.
//!
//! **That is why it survived: the rows came back in input order, not in a wrong
//! order.** A broken sort that scrambles rows gets reported on day one. A broken
//! sort that returns them untouched looks like "the data was already in that
//! order".
//!
//! Statement prep does resolve `ORDER BY 1` — but only when the Nth SELECT item
//! is an *expression*, and a `*` is not one. The parser turns `SELECT unnest(a) x`
//! into `SELECT * FROM unnest(a) x`, so the everyday spelling landed on exactly
//! the shape prep could not resolve. And a FROM-less SELECT (`SELECT
//! upper(unnest(a)) ORDER BY 1`) had no ORDER BY step at all — it returned
//! straight out of the SRF expansion.
//!
//! One resolver now serves all three, and a set-returning item is left alone:
//! copying it into ORDER BY would make the sort key "the whole set", evaluated
//! once per input row — which is the very shape that produced the no-op.

use spg_engine::{Engine, QueryResult};

fn rows_of(e: &mut Engine, sql: &str) -> Vec<String> {
    match e
        .execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"))
    {
        QueryResult::Rows { rows, .. } => rows
            .iter()
            .map(|r| {
                r.values
                    .iter()
                    .map(spg_engine::eval::value_to_text)
                    .collect::<Vec<_>>()
                    .join("|")
            })
            .collect(),
        other => panic!("{sql}: {other:?}"),
    }
}

fn joined(e: &mut Engine, sql: &str) -> String {
    rows_of(e, sql).join(",")
}

#[test]
fn a_positional_order_by_over_an_srf() {
    let mut e = Engine::new();
    // The parser rewrites this into `SELECT * FROM unnest(…) x` — a wildcard
    // projection, which is what statement prep could not resolve.
    assert_eq!(
        joined(&mut e, "SELECT unnest(ARRAY['B','a','A','b']) x ORDER BY 1"),
        "A,B,a,b"
    );
    assert_eq!(
        joined(&mut e, "SELECT unnest(ARRAY['B','a','A','b']) ORDER BY 1"),
        "A,B,a,b"
    );
    assert_eq!(
        joined(
            &mut e,
            "SELECT unnest(ARRAY['B','a','A','b']) x ORDER BY 1 DESC"
        ),
        "b,a,B,A"
    );
    // FROM-less, SRF nested in an expression: this path had no ORDER BY at all.
    assert_eq!(
        joined(&mut e, "SELECT upper(unnest(ARRAY['b','a'])) x ORDER BY 1"),
        "A,B"
    );
    // …and it had no LIMIT/OFFSET either.
    assert_eq!(
        joined(
            &mut e,
            "SELECT upper(unnest(ARRAY['c','a','b'])) x ORDER BY 1 LIMIT 2"
        ),
        "A,B"
    );
    // Naming the column still works, and agrees.
    assert_eq!(
        joined(&mut e, "SELECT unnest(ARRAY['B','a','A','b']) x ORDER BY x"),
        "A,B,a,b"
    );
}

#[test]
fn b_positional_order_by_on_ordinary_shapes() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (a int, b text)").unwrap();
    e.execute("INSERT INTO t VALUES (3,'x'),(1,'z'),(2,'y')")
        .unwrap();
    assert_eq!(joined(&mut e, "SELECT a FROM t ORDER BY 1"), "1,2,3");
    assert_eq!(joined(&mut e, "SELECT a FROM t ORDER BY 1 DESC"), "3,2,1");
    // A wildcard projection: `1` is the first output column, `a`.
    assert_eq!(joined(&mut e, "SELECT * FROM t ORDER BY 1"), "1|z,2|y,3|x");
    // Projection order decides, not table order.
    assert_eq!(
        joined(&mut e, "SELECT b, a FROM t ORDER BY 1"),
        "x|3,y|2,z|1"
    );
    assert_eq!(
        joined(&mut e, "SELECT a, b FROM t ORDER BY 2"),
        "3|x,2|y,1|z"
    );
    // Inside an aggregate, ORDER BY 1 is the CONSTANT 1, not a position (PG).
    assert_eq!(
        joined(&mut e, "SELECT string_agg(a::text, ',' ORDER BY 1) FROM t"),
        "3,1,2"
    );
    assert_eq!(
        joined(&mut e, "SELECT string_agg(a::text, ',' ORDER BY a) FROM t"),
        "1,2,3"
    );
}

#[test]
fn c_set_operations_still_hold() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE s1 (a int)").unwrap();
    e.execute("CREATE TABLE s2 (a int)").unwrap();
    e.execute("INSERT INTO s1 VALUES (1),(2),(2),(NULL)")
        .unwrap();
    e.execute("INSERT INTO s2 VALUES (2),(4)").unwrap();
    assert_eq!(
        joined(&mut e, "SELECT a FROM s1 UNION SELECT a FROM s2 ORDER BY 1"),
        "1,2,4,NULL"
    );
    assert_eq!(
        joined(
            &mut e,
            "SELECT a FROM s1 INTERSECT SELECT a FROM s2 ORDER BY 1"
        ),
        "2"
    );
    assert_eq!(
        joined(
            &mut e,
            "SELECT a FROM s1 EXCEPT SELECT a FROM s2 ORDER BY 1 NULLS LAST"
        ),
        "1,NULL"
    );
    // NULL is its own peer in a set operation (not "unknown").
    assert_eq!(
        rows_of(&mut e, "SELECT NULL::int INTERSECT SELECT NULL::int").len(),
        1
    );
}
