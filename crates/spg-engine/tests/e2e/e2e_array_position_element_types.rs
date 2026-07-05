//! read01 A-group U18 (position subset) — array_position /
//! array_positions element matching + array subscript across every
//! element type, not just Text/Int/BigInt. Previously a `numeric[]` /
//! `date[]` / `bool[]` / `uuid[]` / `bytea[]` / `interval[]` / `money[]`
//! value errored with "first arg must be an array" (the needle-match
//! arms only covered three element types). Element ops now route through
//! the scalar `=` dispatch uniformly. All expected values asserted
//! against live PostgreSQL 18.4.

use spg_engine::{Engine, QueryResult};

fn first(e: &mut Engine, sql: &str) -> spg_storage::Value<'static> {
    let r = e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    let QueryResult::Rows { rows, .. } = r else {
        panic!("expected Rows");
    };
    rows[0].values[0].clone()
}

fn int(e: &mut Engine, sql: &str) -> i32 {
    match first(e, sql) {
        spg_storage::Value::Int(n) => n,
        other => panic!("{sql}: expected Int, got {other:?}"),
    }
}

fn text(e: &mut Engine, sql: &str) -> String {
    match first(e, sql) {
        spg_storage::Value::Text(s) => s.to_string(),
        other => panic!("{sql}: expected Text, got {other:?}"),
    }
}

fn int_array(e: &mut Engine, sql: &str) -> Vec<i32> {
    match first(e, sql) {
        spg_storage::Value::IntArray(items) => items.iter().map(|o| o.unwrap()).collect(),
        other => panic!("{sql}: expected IntArray, got {other:?}"),
    }
}

// Typed-array columns exercise the genuine MoneyArray / UuidArray /
// BytesArray / IntervalArray / BoolArray value shapes (an `ARRAY[..]`
// literal of those element types is inferred as TextArray).
fn typed_cols() -> Engine {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (m money[], u uuid[], b bytea[], iv interval[], bl bool[])")
        .unwrap();
    e.execute(
        "INSERT INTO t VALUES (\
         ARRAY['2'::money, '3'::money], \
         ARRAY['11111111-1111-1111-1111-111111111111'::uuid, \
               '22222222-2222-2222-2222-222222222222'::uuid], \
         ARRAY['\\x01'::bytea, '\\x02'::bytea], \
         ARRAY[INTERVAL '1 day', INTERVAL '2 hours'], \
         ARRAY[true, false, NULL])",
    )
    .unwrap();
    e
}

// ---- array_position across literal-typed element types (PG18.4 = 2) ----

#[test]
fn array_position_numeric() {
    let mut e = Engine::new();
    assert_eq!(
        int(&mut e, "SELECT array_position(ARRAY[1.5,2.5,3.5]::numeric[], 2.5)"),
        2
    );
}

#[test]
fn array_position_float8() {
    let mut e = Engine::new();
    assert_eq!(
        int(&mut e, "SELECT array_position(ARRAY[3.5,2.5]::float8[], 2.5)"),
        2
    );
}

#[test]
fn array_position_date() {
    let mut e = Engine::new();
    assert_eq!(
        int(
            &mut e,
            "SELECT array_position(ARRAY[DATE '2024-01-01', DATE '2024-06-01'], DATE '2024-06-01')"
        ),
        2
    );
}

#[test]
fn array_position_not_found_is_null() {
    let mut e = Engine::new();
    assert!(matches!(
        first(&mut e, "SELECT array_position(ARRAY[1.5,2.5]::numeric[], 9.9)"),
        spg_storage::Value::Null
    ));
}

// ---- array_position across typed-column element types (PG18.4 = 2) ----

#[test]
fn array_position_money() {
    let mut e = typed_cols();
    assert_eq!(int(&mut e, "SELECT array_position(m, '3'::money) FROM t"), 2);
}

#[test]
fn array_position_uuid() {
    let mut e = typed_cols();
    assert_eq!(
        int(
            &mut e,
            "SELECT array_position(u, '22222222-2222-2222-2222-222222222222'::uuid) FROM t"
        ),
        2
    );
}

#[test]
fn array_position_bytea() {
    let mut e = typed_cols();
    assert_eq!(
        int(&mut e, "SELECT array_position(b, '\\x02'::bytea) FROM t"),
        2
    );
}

#[test]
fn array_position_interval() {
    let mut e = typed_cols();
    assert_eq!(
        int(&mut e, "SELECT array_position(iv, INTERVAL '2 hours') FROM t"),
        2
    );
}

#[test]
fn array_position_bool() {
    let mut e = typed_cols();
    assert_eq!(int(&mut e, "SELECT array_position(bl, false) FROM t"), 2);
}

// ---- NULL search value: IS NOT DISTINCT FROM finds the NULL hole ----

#[test]
fn array_position_bool_null_needle_finds_null_hole() {
    let mut e = typed_cols();
    // PG18.4: array_position(ARRAY[true,false,NULL], NULL) = 3.
    assert_eq!(int(&mut e, "SELECT array_position(bl, NULL) FROM t"), 3);
}

// ---- array_positions across element types ----

#[test]
fn array_positions_numeric_multiple() {
    let mut e = Engine::new();
    // PG18.4: array_positions(ARRAY[1.5,2.5,2.5]::numeric[], 2.5) = {2,3}.
    assert_eq!(
        int_array(&mut e, "SELECT array_positions(ARRAY[1.5,2.5,2.5]::numeric[], 2.5)"),
        [2, 3]
    );
}

#[test]
fn array_positions_date_null_needle() {
    let mut e = Engine::new();
    // PG18.4: NULL search value collects the NULL element positions.
    assert_eq!(
        int_array(
            &mut e,
            "SELECT array_positions(ARRAY[DATE '2024-01-01', NULL, NULL], NULL)"
        ),
        [2, 3]
    );
}

// ---- subscript across element types ----

#[test]
fn subscript_numeric() {
    let mut e = Engine::new();
    assert_eq!(text(&mut e, "SELECT ((ARRAY[1.5,2.5,3.5]::numeric[])[3])::text"), "3.5");
}

#[test]
fn subscript_date() {
    let mut e = Engine::new();
    assert_eq!(
        text(
            &mut e,
            "SELECT ((ARRAY[DATE '2024-01-01', DATE '2024-06-01'])[2])::text"
        ),
        "2024-06-01"
    );
}

#[test]
fn subscript_bool_column() {
    let mut e = typed_cols();
    // PG18.4: (ARRAY[true,false,NULL])[1] = t.
    match first(&mut e, "SELECT bl[1] FROM t") {
        spg_storage::Value::Bool(true) => {}
        other => panic!("got {other:?}"),
    }
}

#[test]
fn subscript_out_of_range_is_null() {
    let mut e = Engine::new();
    assert!(matches!(
        first(&mut e, "SELECT (ARRAY[1.5,2.5]::numeric[])[9]"),
        spg_storage::Value::Null
    ));
}
