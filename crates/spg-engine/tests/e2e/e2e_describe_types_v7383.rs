//! 7.38.3 — Describe types, found by asking sentori's own statements.
//!
//! Rather than wait for the next report, all 211 distinct SQL literals
//! in their source were extracted and Described against both SPG and
//! PostgreSQL 18 over their real 17-migration schema. Thirteen answers
//! differed; these are the four causes, each measured against PG18
//! before it was implemented. They sit on pages their suite has not
//! reached yet.

use spg_engine::Engine;

fn described(e: &Engine, sql: &str) -> Vec<(String, String)> {
    let stmt = spg_sql::parser::parse_statement(sql).expect("parse");
    let (_, cols) = e.describe_prepared(&stmt);
    cols.iter()
        .map(|c| (c.name.clone(), format!("{:?}", c.ty)))
        .collect()
}

fn names(e: &Engine, sql: &str) -> Vec<String> {
    described(e, sql).into_iter().map(|(n, _)| n).collect()
}

#[test]
fn pin_v7383_unaliased_function_takes_the_function_name() {
    // PG names an unaliased function call after the function; only an
    // operator expression is `?column?`. SPG's execute path already did
    // this, so Describe and the RowDescription that arrived on Execute
    // disagreed with each other.
    let mut e = Engine::new();
    e.execute("CREATE TABLE an (a INT, t TEXT)").unwrap();
    assert_eq!(names(&e, "SELECT count(*) FROM an"), vec!["count"]);
    assert_eq!(names(&e, "SELECT COUNT(*) FROM an"), vec!["count"]);
    assert_eq!(names(&e, "SELECT max(a) FROM an"), vec!["max"]);
    assert_eq!(names(&e, "SELECT lower(t) FROM an"), vec!["lower"]);
    assert_eq!(names(&e, "SELECT count(DISTINCT a) FROM an"), vec!["count"]);
    // An operator expression stays ?column?, as PG has it.
    assert_eq!(names(&e, "SELECT a + 1 FROM an"), vec!["?column?"]);
    // An alias always wins.
    assert_eq!(names(&e, "SELECT count(*) AS n FROM an"), vec!["n"]);
}

#[test]
fn pin_v7383_json_operators_that_do_not_return_json() {
    // `->>` and `#>>` are TEXT and the predicates are BOOLEAN; the
    // fallback took the LEFT operand's type and called them all JSONB,
    // so a typed decode into String met a jsonb OID.
    let mut e = Engine::new();
    e.execute("CREATE TABLE jt (id INT, payload JSONB)")
        .unwrap();
    for (sql, want) in [
        ("SELECT payload ->> 'k' AS v FROM jt", "Text"),
        ("SELECT payload #>> '{a,b}' AS v FROM jt", "Text"),
        ("SELECT payload -> 'k' AS v FROM jt", "Jsonb"),
        ("SELECT payload ? 'k' AS v FROM jt", "Bool"),
        ("SELECT payload @> '{}' AS v FROM jt", "Bool"),
    ] {
        assert_eq!(described(&e, sql)[0].1, want, "{sql}");
    }
}

#[test]
fn pin_v7383_array_agg_keeps_the_element_type() {
    // array_agg(uuid_col) described as TEXT[] where PG says UUID[], so
    // a decode into Vec<Uuid> failed at the client.
    let mut e = Engine::new();
    e.execute("CREATE TABLE ag (u UUID, i INT, b BOOLEAN, ts TIMESTAMPTZ)")
        .unwrap();
    for (sql, want) in [
        ("SELECT array_agg(u) AS v FROM ag", "UuidArray"),
        ("SELECT array_agg(i) AS v FROM ag", "IntArray"),
        ("SELECT array_agg(b) AS v FROM ag", "BoolArray"),
        ("SELECT array_agg(ts) AS v FROM ag", "TimestamptzArray"),
    ] {
        assert_eq!(described(&e, sql)[0].1, want, "{sql}");
    }
}

#[test]
fn pin_v7383_ordered_set_aggregates_describe() {
    // percentile_cont is double precision; percentile_disc and mode
    // carry the type of the column they order by. Unknown to the
    // function-type map, they made the WHOLE statement describe empty.
    let mut e = Engine::new();
    e.execute("CREATE TABLE ps (f DOUBLE PRECISION, t TEXT)")
        .unwrap();
    assert_eq!(
        described(
            &e,
            "SELECT percentile_cont(0.5) WITHIN GROUP (ORDER BY f) AS p FROM ps"
        )[0]
        .1,
        "Float"
    );
    assert_eq!(
        described(
            &e,
            "SELECT percentile_disc(0.5) WITHIN GROUP (ORDER BY t) AS p FROM ps"
        )[0]
        .1,
        "Text"
    );
    assert_eq!(
        described(&e, "SELECT mode() WITHIN GROUP (ORDER BY t) AS p FROM ps")[0].1,
        "Text"
    );
}

#[test]
fn pin_v7383_set_returning_function_in_from_describes() {
    // A set-returning function as a FROM item made the statement
    // describe nothing at all; the parser had already resolved the
    // column name, only the type was missing.
    let mut e = Engine::new();
    e.execute("CREATE TABLE ev (id INT, payload JSONB)")
        .unwrap();
    assert_eq!(
        names(
            &e,
            "SELECT key FROM ev, LATERAL jsonb_object_keys(payload -> 'context') AS key"
        ),
        vec!["key"]
    );
    assert_eq!(
        names(&e, "SELECT v FROM unnest(ARRAY[1,2,3]) AS v"),
        vec!["v"]
    );
}
