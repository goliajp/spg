//! v7.39 (read01 round 76) — five holes a differential sweep of the JSON /
//! table-function surface turned up, each one a *decision that was never made*
//! standing behind something that looked finished:
//!
//!   1. `FROM jsonb_each(j) AS t(k, v)` — the column-alias list was parsed and
//!      dropped on the floor (`let (alias, _column_aliases) = …`). The very same
//!      function in LATERAL position honoured it. One function, two entries, one
//!      of them wired.
//!   2. `to_jsonb(ARRAY[[1,2]])` → the JSON *string* `"{{1,2}}"`. The encoder had
//!      arms for text/int/bigint arrays and a catch-all that quotes anything else
//!      as text — so every other element type (bool, float, date, …) and every
//!      matrix came out as a quoted string instead of a JSON array.
//!   3. `array_to_json(ARRAY[[1,2]])` → "needs array, got IntArray2D". An error
//!      that accuses the caller of not passing an array while naming an array
//!      type is not a type error, it is a missing arm wearing one.
//!   4. `generate_series(date, date, interval)` came back `timestamp`. PG has no
//!      date overload and prefers the timestamptz candidate, so the rows carry a
//!      `+00` offset.
//!   5. `jsonb_populate_record(NULL::t, j)` — the canonical PG spelling, where
//!      the row shape comes from the base argument's type. SPG demanded a column
//!      list the PG form does not have.
//!
//! And the one the fix for (4) uncovered, which is the widest of them: a cast
//! inside an aggregate argument, or over an aggregate result, is driven by the
//! compiled step VM, whose `Step::Cast` calls a *pure* `cast_value(value,
//! target)`. Rendering a timestamptz needs the expression's STATIC type (the
//! runtime value is a tz-less `Value::Timestamp`), so the offset silently
//! vanished for `string_agg(x::text, ',')` and `min(x)::text` — on real tables,
//! not just synthetic ones. The fast path had quietly opted out of a decision
//! the interpreter makes.

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

#[test]
fn a_table_function_column_alias_list() {
    let mut e = Engine::new();
    // PG: a=1,b="x" — the plain (non-_text) form keeps JSON rendering.
    assert_eq!(
        r1(
            &mut e,
            "SELECT string_agg(k || '=' || v::text, ',' ORDER BY k) \
             FROM jsonb_each('{\"a\":1,\"b\":\"x\"}'::jsonb) AS t(k, v)"
        ),
        "a=1,b=\"x\""
    );
    assert_eq!(
        r1(
            &mut e,
            "SELECT string_agg(k || '=' || v, ',' ORDER BY k) \
             FROM jsonb_each_text('{\"a\":\"1\",\"b\":\"x\"}'::jsonb) AS t(k, v)"
        ),
        "a=1,b=x"
    );
    // Not renaming still gives the declared key/value names.
    assert_eq!(
        r1(
            &mut e,
            "SELECT string_agg(key || '=' || value::text, ',' ORDER BY key) \
             FROM jsonb_each('{\"a\":1}'::jsonb)"
        ),
        "a=1"
    );
}

#[test]
fn b_every_array_encodes_as_a_json_array() {
    let mut e = Engine::new();
    assert_eq!(
        r1(&mut e, "SELECT array_to_json(ARRAY[[1,2],[3,4]])"),
        "[[1,2],[3,4]]"
    );
    assert_eq!(r1(&mut e, "SELECT to_jsonb(ARRAY[[1,2]])"), "[[1, 2]]");
    // The element types the old per-variant match never had an arm for.
    assert_eq!(
        r1(&mut e, "SELECT to_jsonb(ARRAY[true,false])"),
        "[true, false]"
    );
    assert_eq!(
        r1(&mut e, "SELECT array_to_json(ARRAY[1.5::float8, 2.5])"),
        "[1.5,2.5]"
    );
    assert_eq!(
        r1(&mut e, "SELECT to_json(ARRAY['2020-01-01'::date])"),
        "[\"2020-01-01\"]"
    );
}

#[test]
fn c_jsonb_ordering_operators() {
    let mut e = Engine::new();
    // The total order ORDER BY already sorted by; the binary operators were
    // simply never routed to it.
    assert_eq!(r1(&mut e, "SELECT jsonb '\"a\"' < jsonb '\"b\"'"), "true");
    // PG's type-class ladder: Object > Array.
    assert_eq!(r1(&mut e, "SELECT jsonb '{\"a\":1}' > jsonb '[1]'"), "true");
    assert_eq!(r1(&mut e, "SELECT jsonb '1' <= jsonb '1'"), "true");
    assert_eq!(r1(&mut e, "SELECT jsonb '2' < jsonb '1'"), "false");
}

#[test]
fn d_generate_series_date_bounds_are_timestamptz() {
    let mut e = Engine::new();
    assert_eq!(
        r1(
            &mut e,
            "SELECT pg_typeof(d) FROM generate_series('2020-01-01'::date, '2020-01-02'::date, \
             '1 day'::interval) d LIMIT 1"
        ),
        "timestamp with time zone"
    );
    assert_eq!(
        r1(
            &mut e,
            "SELECT string_agg(d::text, ',') FROM generate_series('2020-01-01'::date, \
             '2020-01-02'::date, '1 day'::interval) d"
        ),
        "2020-01-01 00:00:00+00,2020-01-02 00:00:00+00"
    );
    // A genuinely timestamp-typed bound keeps the TZ-naive result type.
    assert_eq!(
        r1(
            &mut e,
            "SELECT string_agg(d::text, ',') FROM generate_series('2020-01-01'::timestamp, \
             '2020-01-02'::timestamp, '1 day'::interval) d"
        ),
        "2020-01-01 00:00:00,2020-01-02 00:00:00"
    );
}

#[test]
fn e_timestamptz_keeps_its_offset_through_the_compiled_vm() {
    let mut e = Engine::new();
    ok(&mut e, "CREATE TABLE tz (x timestamptz)");
    ok(&mut e, "INSERT INTO tz VALUES ('2020-01-01')");
    // Plain projection always worked — the interpreter drives it.
    assert_eq!(
        r1(&mut e, "SELECT x::text FROM tz"),
        "2020-01-01 00:00:00+00"
    );
    // Cast INSIDE an aggregate argument: compiled step VM.
    assert_eq!(
        r1(&mut e, "SELECT string_agg(x::text, ',') FROM tz"),
        "2020-01-01 00:00:00+00"
    );
    assert_eq!(
        r1(&mut e, "SELECT max(x::text) FROM tz"),
        "2020-01-01 00:00:00+00"
    );
    // Cast OVER an aggregate result: compiled too, over the synthetic schema.
    assert_eq!(
        r1(&mut e, "SELECT min(x)::text FROM tz"),
        "2020-01-01 00:00:00+00"
    );
}

#[test]
fn f_populate_record_takes_its_shape_from_the_base_type() {
    let mut e = Engine::new();
    ok(&mut e, "CREATE TABLE pr (a int, b text)");
    assert_eq!(
        r1(
            &mut e,
            "SELECT string_agg(a::text || '/' || coalesce(b, '-'), ',') \
             FROM jsonb_populate_record(NULL::pr, '{\"a\":1,\"b\":\"x\"}'::jsonb) AS t"
        ),
        "1/x"
    );
    // A key the JSON does not carry is NULL, not an error.
    assert_eq!(
        r1(
            &mut e,
            "SELECT string_agg(a::text || '/' || coalesce(b, '-'), ',') \
             FROM jsonb_populate_recordset(NULL::pr, '[{\"a\":1,\"b\":\"x\"},{\"a\":2}]'::jsonb) AS t"
        ),
        "1/x,2/-"
    );
    assert_eq!(
        r1(
            &mut e,
            "SELECT string_agg(a::text, ',') FROM json_populate_record(NULL::pr, '{\"a\":7}'::json) AS t"
        ),
        "7"
    );
    // An empty set is zero rows, not one all-NULL row.
    assert_eq!(
        r1(
            &mut e,
            "SELECT count(*)::text FROM jsonb_populate_recordset(NULL::pr, '[]'::jsonb) AS t"
        ),
        "0"
    );
    // The column-definition form (base type `record`) still works.
    assert_eq!(
        r1(
            &mut e,
            "SELECT string_agg(a::text || b, ',') \
             FROM jsonb_to_record('{\"a\":1,\"b\":\"z\"}'::jsonb) AS t(a int, b text)"
        ),
        "1z"
    );
}
