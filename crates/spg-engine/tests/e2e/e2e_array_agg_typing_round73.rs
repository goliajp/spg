//! v7.39 (read01 round 73) — `array_agg`'s element type, and the 2-D regression
//! this campaign's own fix introduced.
//!
//! Rounds 71 and 72 killed the "fallback standing where a decision should be"
//! pattern in the array literal and the array functions. `array_agg` was the
//! fifth site of the same pattern — its finalize dispatched on the first
//! non-NULL element with arms for int and bigint and text for everything else,
//! so `array_agg(bool_col)` came back as `text[]`. There is now ONE builder,
//! shared by the literal, `array_agg` and the ordered-set aggregates.
//!
//! Two things the sweep caught that were NOT in the plan:
//!
//!   1. **A regression round 72 introduced.** Giving `ARRAY[true,false]` its real
//!      `bool[]` type broke the 2-D detector, which only knew Int / BigInt / Text
//!      rows: `ARRAY[ARRAY[true,false]]` stopped being 2-D at all and collapsed
//!      into a 1-D text[] of rendered rows, with `[1][2]` failing outright. No
//!      test covered a 2-D bool array, so the gate was green. The differential
//!      that was chasing round 72's own residual is what found it.
//!   2. **PG QUOTES an array element containing whitespace.** `array_agg(interval)`
//!      now builds a real `interval[]`, and its renderer never quoted — because
//!      none of the typed arrays it served had ever produced an element with a
//!      space in it. `{"1 day",02:00:00}`, not `{1 day,02:00:00}`.
//!
//! Byte-locked against live PG18.4.

use spg_engine::{Engine, QueryResult};

fn ok(e: &mut Engine, sql: &str) {
    e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err:?}"));
}

fn r1(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => spg_engine::eval::value_to_text(&rows[0].values[0]),
        other => panic!("{sql}: {other:?}"),
    }
}

fn seeded() -> Engine {
    let mut e = Engine::new();
    ok(&mut e, "CREATE TABLE t (id int, b bool, d date, n numeric, v text)");
    ok(
        &mut e,
        "INSERT INTO t VALUES (1,true,'2024-01-01',1.5,'a'),(2,false,'2024-02-01',2.5,'b')",
    );
    e
}

#[test]
fn array_agg_keeps_the_element_type() {
    let mut e = seeded();
    assert_eq!(r1(&mut e, "SELECT pg_typeof(array_agg(b)) FROM t"), "boolean[]");
    assert_eq!(r1(&mut e, "SELECT pg_typeof(array_agg(d)) FROM t"), "date[]");
    assert_eq!(r1(&mut e, "SELECT pg_typeof(array_agg(n)) FROM t"), "numeric[]");
    assert_eq!(r1(&mut e, "SELECT pg_typeof(array_agg(v)) FROM t"), "text[]");
    assert_eq!(r1(&mut e, "SELECT pg_typeof(array_agg(id)) FROM t"), "integer[]");
}

#[test]
fn an_aggregated_array_reaches_the_array_functions() {
    // The point of the type: a Bool needle can only match a BoolArray element.
    let mut e = seeded();
    assert_eq!(
        r1(
            &mut e,
            "SELECT array_to_string(array_remove(array_agg(b ORDER BY id), false),'|') FROM t"
        ),
        "t"
    );
    assert_eq!(
        r1(&mut e, "SELECT array_to_string(array_agg(b ORDER BY id),'|') FROM t"),
        "t|f"
    );
}

#[test]
fn a_two_dimensional_bool_array_is_still_two_dimensional() {
    // The round-72 regression: this collapsed into a 1-D text[] and `[1][2]`
    // errored with "subscript target must be an array".
    let mut e = Engine::new();
    assert_eq!(r1(&mut e, "SELECT (ARRAY[ARRAY[true,false]])[1][2]::text"), "false");
    assert_eq!(r1(&mut e, "SELECT pg_typeof(ARRAY[ARRAY[1,2],ARRAY[3,4]])"), "integer[]");
    assert_eq!(r1(&mut e, "SELECT (ARRAY[ARRAY[1,2],ARRAY[3,4]])[2][1]::text"), "3");
}

#[test]
fn an_array_element_with_a_space_is_quoted() {
    // PG's array output quotes any element carrying a delimiter or whitespace.
    let mut e = Engine::new();
    ok(&mut e, "CREATE TABLE iv (id int, i interval)");
    ok(
        &mut e,
        "INSERT INTO iv VALUES (1,'1 day'::interval),(2,'2 hours'::interval)",
    );
    assert_eq!(
        r1(&mut e, "SELECT array_agg(i ORDER BY id)::text FROM iv"),
        "{\"1 day\",02:00:00}"
    );
}
