//! read01 round 324 (V42) — "invalid input syntax" reads like PG's.
//!
//! The ledger recorded one case (a malformed timestamp literal). Measuring
//! the family showed the divergence was wider, and in three directions:
//!
//!   * SPG's own phrasing where PG has a settled one — `cannot parse "x"
//!     as TIMESTAMP (expected YYYY-MM-DD…)` for PG's `invalid input syntax
//!     for type timestamp: "x"`, and the same for interval and bigint;
//!   * the WRONG TYPE NAME — a `::timestamptz` cast reported `TIMESTAMP`;
//!   * PG's second shape missing entirely: a value that IS date-shaped but
//!     carries an impossible field is `date/time field value out of range`,
//!     not an input-syntax error, and it carries a DateStyle HINT when the
//!     month or day is outside its universal range.
//!
//! Every expectation was read off live PG 18.4.

use spg_engine::Engine;

fn err(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql) {
        Ok(v) => panic!("{sql}: expected an error, got {v:?}"),
        // The engine's own layer prefix is stripped by both wires (r323).
        Err(x) => format!("{x}")
            .trim_start_matches("eval: ")
            .trim_start_matches("type mismatch: ")
            .to_string(),
    }
}

#[test]
fn an_unparseable_datetime_literal_uses_pgs_wording() {
    let mut e = Engine::new();
    assert_eq!(
        err(&mut e, "SELECT 'not-a-date'::timestamp"),
        "invalid input syntax for type timestamp: \"not-a-date\"",
    );
    assert_eq!(
        err(&mut e, "SELECT ''::timestamp"),
        "invalid input syntax for type timestamp: \"\"",
    );
    // Date-shaped but with junk attached is still a syntax error in PG.
    assert_eq!(
        err(&mut e, "SELECT '2020-01-01 abc'::timestamp"),
        "invalid input syntax for type timestamp: \"2020-01-01 abc\"",
    );
    // …and so is a half-written date.
    assert_eq!(
        err(&mut e, "SELECT '2020-01'::timestamp"),
        "invalid input syntax for type timestamp: \"2020-01\"",
    );
}

/// The zone-carrying type names itself. This arm reported `TIMESTAMP`.
#[test]
fn timestamptz_names_its_own_type() {
    let mut e = Engine::new();
    assert_eq!(
        err(&mut e, "SELECT 'not-a-date'::timestamptz"),
        "invalid input syntax for type timestamp with time zone: \"not-a-date\"",
    );
}

/// PG's second shape: shaped like a date, impossible field value.
#[test]
fn an_impossible_field_is_out_of_range_not_bad_syntax() {
    let mut e = Engine::new();
    // Month / day outside their UNIVERSAL range — a different DateStyle
    // could have explained it, so PG adds the hint.
    for lit in ["2020-13-01", "2020-01-32", "2020-00-01", "2020-01-00"] {
        assert_eq!(
            err(&mut e, &format!("SELECT '{lit}'::timestamp")),
            format!(
                "date/time field value out of range: \"{lit}\"\n\
                 HINT:  Perhaps you need a different \"DateStyle\" setting."
            ),
        );
    }
    // A day that is fine for the field but not for that month, and a
    // time-of-day overflow, get NO hint — measured.
    for lit in ["2020-02-30", "2020-01-01 25:00:00", "2020-01-01 12:61:00"] {
        assert_eq!(
            err(&mut e, &format!("SELECT '{lit}'::timestamp")),
            format!("date/time field value out of range: \"{lit}\""),
            "no DateStyle hint for `{lit}`"
        );
    }
}

#[test]
fn interval_and_bigint_use_pgs_wording() {
    let mut e = Engine::new();
    assert_eq!(
        err(&mut e, "SELECT 'nope'::interval"),
        "invalid input syntax for type interval: \"nope\"",
    );
    assert_eq!(
        err(&mut e, "SELECT 'abc'::bigint"),
        "invalid input syntax for type bigint: \"abc\"",
    );
}

/// The coercion path (an INSERT into a typed column) carried a
/// ``(column `x`)`` suffix PG has none of — PG points with the caret
/// instead, and its message is the bare one. Measured on PG 18.4 for
/// every type below.
#[test]
fn the_insert_path_carries_no_column_suffix() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (a TIME, b UUID, c INTERVAL, d POINT, e MONEY)")
        .unwrap();
    for (col, ty) in [
        ("a", "time"),
        ("b", "uuid"),
        ("c", "interval"),
        ("d", "point"),
        ("e", "money"),
    ] {
        let msg = err(&mut e, &format!("INSERT INTO t ({col}) VALUES ('abc')"));
        assert_eq!(
            msg,
            format!("invalid input syntax for type {ty}: \"abc\""),
            "column {col}"
        );
    }
}
