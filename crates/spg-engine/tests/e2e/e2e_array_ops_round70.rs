//! v7.39 (read01 access/, round 70) — `tags @> '{b}'`: the array operators
//! beside a bare string literal.
//!
//! The round set out to audit `access/` — index semantics. Two probes said the
//! indexes are honest: an expression index, a partial index, a DESC/NULLS index,
//! `text_pattern_ops`, `INCLUDE`, `CONCURRENTLY`, `REINDEX`, a `COLLATE "C"`
//! index and two GIN indexes all give byte-identical RESULTS to PG. What the GIN
//! probe fell over was not the index at all:
//!
//!     SELECT * FROM t WHERE tags @> '{b}';
//!     ERROR:  JSON @>: left side must be JSON or TEXT, got Some(TextArray)
//!
//! PG reads a bare string literal beside an ARRAY operand as an array of that
//! type — an "unknown" literal takes the other side's type. SPG saw a TEXT and
//! routed the operator to its JSON reading; `@>` and `<@` errored, and `&&` went
//! to the INET one ("inet operator requires INET/CIDR/TEXT operands").
//!
//! The tell: `tags @> ARRAY['b']` worked all along. The operators were right —
//! only the literal coercion was missing.

use spg_engine::{Engine, QueryResult};

fn ok(e: &mut Engine, sql: &str) {
    e.execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"));
}

fn r1(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => spg_engine::eval::value_to_text(&rows[0].values[0]),
        other => panic!("{sql}: {other:?}"),
    }
}

fn seeded() -> Engine {
    let mut e = Engine::new();
    ok(&mut e, "CREATE TABLE t (id int, tags text[], nums int[])");
    ok(
        &mut e,
        "INSERT INTO t VALUES (1,'{a,b}','{1,2}'),(2,'{b,c}','{2,3}'),(3,'{c}','{3}')",
    );
    e
}

#[test]
fn contains_takes_a_string_literal_as_an_array() {
    let mut e = seeded();
    assert_eq!(
        r1(
            &mut e,
            "SELECT string_agg(id::text, ',' ORDER BY id) FROM t WHERE tags @> '{b}'"
        ),
        "1,2"
    );
    // The element-typed form was never broken — this is the control.
    assert_eq!(
        r1(
            &mut e,
            "SELECT string_agg(id::text, ',' ORDER BY id) FROM t WHERE tags @> ARRAY['b']"
        ),
        "1,2"
    );
}

#[test]
fn contained_by_and_overlap_too() {
    let mut e = seeded();
    assert_eq!(
        r1(
            &mut e,
            "SELECT string_agg(id::text, ',' ORDER BY id) FROM t WHERE tags <@ '{a,b,c}'"
        ),
        "1,2,3"
    );
    // `&&` used to reach the INET operator with an array on the left.
    assert_eq!(
        r1(
            &mut e,
            "SELECT string_agg(id::text, ',' ORDER BY id) FROM t WHERE tags && '{a,c}'"
        ),
        "1,2,3"
    );
}

#[test]
fn an_int_array_column_coerces_the_same_way() {
    let mut e = seeded();
    assert_eq!(
        r1(
            &mut e,
            "SELECT string_agg(id::text, ',' ORDER BY id) FROM t WHERE nums @> '{2}'"
        ),
        "1,2"
    );
    assert_eq!(
        r1(
            &mut e,
            "SELECT string_agg(id::text, ',' ORDER BY id) FROM t WHERE nums && '{3}'"
        ),
        "2,3"
    );
}

#[test]
fn the_json_reading_of_the_same_operator_still_works() {
    // A text that is not an array literal is left alone, so `@>` can still be
    // the JSON containment operator.
    let mut e = Engine::new();
    ok(&mut e, "CREATE TABLE j (id int, doc jsonb)");
    ok(
        &mut e,
        "INSERT INTO j VALUES (1,'{\"k\":1}'),(2,'{\"k\":2}')",
    );
    assert_eq!(
        r1(
            &mut e,
            "SELECT string_agg(id::text, ',') FROM j WHERE doc @> '{\"k\":1}'"
        ),
        "1"
    );
}

#[test]
fn a_gin_index_does_not_change_the_answer() {
    let mut e = seeded();
    ok(&mut e, "CREATE INDEX i_gin ON t USING gin (tags)");
    assert_eq!(
        r1(
            &mut e,
            "SELECT string_agg(id::text, ',' ORDER BY id) FROM t WHERE tags @> '{b}'"
        ),
        "1,2"
    );
}
