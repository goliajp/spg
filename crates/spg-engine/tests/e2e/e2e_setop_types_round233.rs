//! v7.39 (round 233) — set-operation column type resolution, the gap
//! round 232 scoped out. PG resolves each result column to one type before
//! merging anything and refuses the query when the branches have no common
//! type. SPG's unifier (`unify_union_columns`) is value-driven and
//! deliberately conservative — its own comment says a column where any
//! cell fails to coerce "is left exactly as it was ... this never turns a
//! previously-working query into an error" — so a mismatch produced a
//! column HOLDING BOTH TYPES instead of an error.
//!
//! The rule can't be read off the schemas alone: SPG has no `Unknown`
//! DataType, so a bare `'a'` literal describes as TEXT exactly like a real
//! text column, while PG treats the two completely differently. The check
//! reads the branch ASTs for that reason. Unification table and every
//! expectation probed against live PG18.4 (2026-07-19).

use spg_engine::{Engine, QueryResult};

fn seeded() -> Engine {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (a int, b text)").unwrap();
    e.execute("INSERT INTO t VALUES (1,'x'),(2,'y'),(3,NULL)")
        .unwrap();
    e
}

fn err(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql) {
        Err(x) => format!("{x}"),
        Ok(ok) => panic!("{sql}: expected an error, got {ok:?}"),
    }
}

fn col(e: &mut Engine, sql: &str) -> Vec<String> {
    match e.execute(sql).unwrap_or_else(|x| panic!("{sql}: {x:?}")) {
        QueryResult::Rows { rows, .. } => rows
            .iter()
            .map(|r| match &r.values[0] {
                spg_storage::Value::Null => String::new(),
                v => spg_engine::eval::value_to_text(v),
            })
            .collect(),
        other => panic!("{sql}: {other:?}"),
    }
}

#[test]
fn branches_with_no_common_type_are_refused() {
    let mut e = seeded();
    for (sql, want) in [
        (
            "SELECT a, b FROM t UNION SELECT b, a FROM t",
            "UNION types integer and text cannot be matched",
        ),
        (
            "SELECT a FROM t UNION SELECT 'q'::text",
            "UNION types integer and text cannot be matched",
        ),
        (
            "SELECT 'q'::text UNION SELECT 1",
            "UNION types text and integer cannot be matched",
        ),
        (
            "SELECT 1 UNION SELECT true",
            "UNION types integer and boolean cannot be matched",
        ),
        (
            "SELECT a FROM t EXCEPT SELECT b FROM t",
            "EXCEPT types integer and text cannot be matched",
        ),
        (
            "SELECT a FROM t INTERSECT SELECT b FROM t",
            "INTERSECT types integer and text cannot be matched",
        ),
    ] {
        let got = err(&mut e, sql);
        assert!(got.contains(want), "{sql}\n  want {want:?}\n  got  {got:?}");
    }
}

#[test]
fn branches_within_a_type_family_still_unify() {
    let mut e = seeded();
    // Numeric family widens; string family collapses to text; date/time
    // family widens. (Probed: int ∪ bigint → bigint, text ∪ varchar →
    // text, date ∪ timestamp → timestamp.)
    assert_eq!(col(&mut e, "SELECT 1 UNION SELECT 2::bigint ORDER BY 1"), ["1", "2"]);
    assert_eq!(
        col(&mut e, "SELECT 1 UNION SELECT 2.5 ORDER BY 1"),
        ["1", "2.5"]
    );
    assert_eq!(
        col(
            &mut e,
            "SELECT 'a'::text UNION SELECT 'b'::varchar(4) ORDER BY 1"
        ),
        ["a", "b"]
    );
    assert_eq!(
        col(
            &mut e,
            "SELECT DATE '2024-01-02' UNION SELECT TIMESTAMP '2024-01-01 00:00:00' ORDER BY 1"
        )
        .len(),
        2
    );
    // The everyday shape must keep working.
    assert_eq!(col(&mut e, "SELECT a FROM t UNION SELECT a FROM t ORDER BY 1"), ["1", "2", "3"]);
}

#[test]
fn an_untyped_literal_takes_the_other_branchs_type() {
    let mut e = seeded();
    // Converts: the literal becomes an integer, in both directions.
    assert_eq!(col(&mut e, "SELECT 1 UNION SELECT '2' ORDER BY 1"), ["1", "2"]);
    assert_eq!(col(&mut e, "SELECT '2' UNION SELECT 1 ORDER BY 1"), ["1", "2"]);
    // Doesn't convert: PG reports the value, not a type mismatch — the
    // distinction the AST-level unknown check exists for.
    for sql in [
        "SELECT 1 UNION SELECT 'a'",
        "SELECT 'a' UNION SELECT 1",
        "SELECT 1 UNION ALL SELECT 'a'",
        "SELECT a FROM t UNION SELECT 'a'",
    ] {
        let got = err(&mut e, sql);
        assert!(
            got.contains("invalid input syntax for type integer: \"a\""),
            "{sql}: {got}"
        );
    }
    let got = err(&mut e, "SELECT true UNION SELECT 'a'");
    assert!(
        got.contains("invalid input syntax for type boolean: \"a\""),
        "{got}"
    );
    // NULL is untyped too, and takes the other side without complaint.
    // (Two rows, 1 and NULL — a set operation without ORDER BY has no
    // guaranteed order, so sort before comparing.)
    let mut got = col(&mut e, "SELECT NULL UNION SELECT 1");
    got.sort();
    assert_eq!(got, ["", "1"]);
    // Two untyped branches stay text.
    assert_eq!(col(&mut e, "SELECT 'a' UNION SELECT 'b' ORDER BY 1"), ["a", "b"]);
    // A chain resolves left to right.
    let got = err(&mut e, "SELECT 1 UNION SELECT NULL UNION SELECT 'x'");
    assert!(
        got.contains("invalid input syntax for type integer: \"x\""),
        "{got}"
    );
}
