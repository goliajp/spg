//! v7.39 (read01 round 90) — INSERT text→typed coercion error messages aligned
//! to PG.
//!
//! Gate first (per methodology): does SPG reject VALID coercions? No — a probe
//! against live PG18.4 confirmed SPG accepts every valid one (`'123'`→int,
//! `'true'`→bool, `' 42 '`→int, `'2020-01-15'`→date, …). So there is no
//! behaviour bug; only the error message for an INVALID value differed.
//!
//! SPG said its own "type mismatch in column X (position P): expected T, got
//! TEXT" / "cannot parse X as DATE for column d", which no client matches. PG's:
//!   * a non-parseable numeric/bool text → `invalid input syntax for type <T>:
//!     "<value>"` (22P02);
//!   * a date-shaped string with an out-of-range field (month 13) →
//!     `date/time field value out of range: "<value>"` (22008);
//!   * a non-date-shaped string → `invalid input syntax for type date: "<value>"`
//!     (22007).

use spg_engine::Engine;

fn err(e: &mut Engine, sql: &str) -> String {
    e.execute(sql).unwrap_err().to_string()
}

#[test]
fn a_valid_coercions_still_accepted() {
    // The behaviour half — must not regress into rejecting valid values.
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (i int, b bigint, n numeric, f float8, bo bool, d date)")
        .unwrap();
    e.execute("INSERT INTO t(i) VALUES ('123')").unwrap();
    e.execute("INSERT INTO t(b) VALUES ('9999999999')").unwrap();
    e.execute("INSERT INTO t(n) VALUES ('3.14')").unwrap();
    e.execute("INSERT INTO t(f) VALUES ('2.5')").unwrap();
    e.execute("INSERT INTO t(bo) VALUES ('true')").unwrap();
    e.execute("INSERT INTO t(bo) VALUES ('t')").unwrap();
    e.execute("INSERT INTO t(i) VALUES (' 42 ')").unwrap();
    e.execute("INSERT INTO t(d) VALUES ('2020-01-15')").unwrap();
}

#[test]
fn b_invalid_numeric_and_bool_are_invalid_input_syntax() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (i int, b bigint, si smallint, f float8, r real, bo bool)")
        .unwrap();
    assert!(err(&mut e, "INSERT INTO t(i) VALUES ('notint')")
        .contains("invalid input syntax for type integer: \"notint\""));
    // '3.5' is not an integer literal.
    assert!(err(&mut e, "INSERT INTO t(i) VALUES ('3.5')")
        .contains("invalid input syntax for type integer: \"3.5\""));
    assert!(err(&mut e, "INSERT INTO t(b) VALUES ('xx')")
        .contains("invalid input syntax for type bigint: \"xx\""));
    assert!(err(&mut e, "INSERT INTO t(f) VALUES ('nope')")
        .contains("invalid input syntax for type double precision: \"nope\""));
    assert!(err(&mut e, "INSERT INTO t(bo) VALUES ('maybe')")
        .contains("invalid input syntax for type boolean: \"maybe\""));
}

#[test]
fn c_date_out_of_range_vs_invalid_syntax() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (d date, ts timestamp)").unwrap();
    // Date-shaped but a field is out of range → 22008 wording.
    assert!(err(&mut e, "INSERT INTO t(d) VALUES ('2020-13-99')")
        .contains("date/time field value out of range: \"2020-13-99\""));
    assert!(err(&mut e, "INSERT INTO t(d) VALUES ('2020-02-30')")
        .contains("date/time field value out of range: \"2020-02-30\""));
    // Not date-shaped → invalid input syntax.
    assert!(err(&mut e, "INSERT INTO t(d) VALUES ('notadate')")
        .contains("invalid input syntax for type date: \"notadate\""));
    assert!(err(&mut e, "INSERT INTO t(ts) VALUES ('garbage')")
        .contains("invalid input syntax for type timestamp: \"garbage\""));
}
