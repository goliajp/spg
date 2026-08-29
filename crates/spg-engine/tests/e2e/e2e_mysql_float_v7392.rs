//! v7.39.2 — MySQL's float family: the width, the syntax, the digits.
//!
//! Four defects, all measured against MySQL 9.7.2:
//!
//! 1. `DOUBLE(10,2)` failed the whole CREATE with `syntax error at or
//!    near "("`. The guard that accepts the `(m,d)` display form tested
//!    for `float` while its comment claimed both spellings, and every
//!    legacy MySQL schema spells money that way.
//! 2. A bare `FLOAT` is FOUR bytes on MySQL and eight on PostgreSQL
//!    (where it IS `float8`'s spelling). SPG used PostgreSQL's for both,
//!    so a MySQL FLOAT column silently kept precision MySQL drops —
//!    3.14159265358979 reads back `3.14159` there and came back whole
//!    here — and reported itself as `double` to every reflection. This
//!    is the mirror of the REAL split SPG already honoured: MySQL's REAL
//!    is a synonym for DOUBLE.
//! 3. A FLOAT prints to SIX significant digits there, and both widths
//!    stay in FIXED notation for decimal exponents in `[-15, 14]`, so
//!    `123456789` reads `123457000` and `1e15` reads `1e15`. SPG printed
//!    PostgreSQL's shortest round-trip with a much narrower window.
//! 4. The wire had its own copy of that rendering — Rust's `Display` for
//!    f64 and PostgreSQL's `float4out` for f32 — so the engine's rule
//!    could not reach it however the session was configured.
//!
//! Residual, measured and NOT fixed here: the `(m,d)` digits are not a
//! display hint. MySQL ROUNDS on write — 3.14159265358979 into either
//! `FLOAT(10,2)` or `DOUBLE(10,2)` stores 3.14 — and reports
//! `float(10,2)` in `COLUMN_TYPE`. Recording the pair needs a catalog
//! field of its own.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn mysql() -> Engine {
    let mut e = Engine::new();
    e.set_mysql_dialect(true);
    e
}

fn one(e: &mut Engine, sql: &str) -> Value<'static> {
    match e.execute(sql) {
        Ok(QueryResult::Rows { rows, .. }) => rows[0].values[0].clone().into_owned(),
        Ok(other) => panic!("{sql}: {other:?}"),
        Err(err) => panic!("{sql}: {err}"),
    }
}

/// Render through the SESSION, not through a style-less helper:
/// `value_to_text` carries the default RenderStyle, so it answers
/// PostgreSQL's digits however the session is configured, and the first
/// draft of this pin was measuring that helper rather than SPG.
fn text(e: &mut Engine, sql: &str) -> String {
    match one(
        e,
        &sql.replacen("SELECT ", "SELECT CAST(", 1)
            .replacen(" FROM ", " AS CHAR) FROM ", 1),
    ) {
        Value::Text(t) => t.to_string(),
        other => panic!("{sql}: {other:?}"),
    }
}

#[test]
fn the_display_form_parses_for_both_spellings() {
    let mut e = mysql();
    // The one that failed outright.
    e.execute("CREATE TABLE m (a DOUBLE(10,2), b FLOAT(10,2))")
        .expect("DOUBLE(m,d) must parse");
    e.execute("INSERT INTO m VALUES (1.5, 1.5)")
        .expect("insert");
    assert_eq!(one(&mut e, "SELECT a FROM m"), Value::Float(1.5));
    assert_eq!(one(&mut e, "SELECT b FROM m"), Value::Real(1.5));
}

#[test]
fn a_mysql_float_is_four_bytes_and_a_real_is_eight() {
    let mut e = mysql();
    e.execute("CREATE TABLE f (f FLOAT, d DOUBLE, r REAL)")
        .expect("ddl");
    e.execute("INSERT INTO f VALUES (3.14159265358979, 3.14159265358979, 3.14159265358979)")
        .expect("insert");
    // Measured on MySQL 9.7.2: 3.14159 / 3.14159265358979 / …79.
    assert_eq!(text(&mut e, "SELECT f FROM f"), "3.14159");
    assert_eq!(text(&mut e, "SELECT d FROM f"), "3.14159265358979");
    assert_eq!(text(&mut e, "SELECT r FROM f"), "3.14159265358979");
    assert_eq!(
        text(
            &mut e,
            "SELECT column_type FROM information_schema.columns \
             WHERE table_name='f' AND column_name='f'"
        ),
        "float"
    );
}

#[test]
fn the_rendering_is_mysqls() {
    let mut e = mysql();
    e.execute("CREATE TABLE r (x FLOAT, d DOUBLE)")
        .expect("ddl");
    // Every pair below is a MySQL 9.7.2 run.
    for (input, want_float, want_double) in [
        ("3.14159265358979", "3.14159", "3.14159265358979"),
        ("0.1", "0.1", "0.1"),
        ("1.0", "1", "1"),
        ("123456789", "123457000", "123456789"),
        ("16777217", "16777200", "16777217"),
        ("2.718281828", "2.71828", "2.718281828"),
        // The window: fixed through exponent 14, scientific from 15.
        ("1e14", "100000000000000", "100000000000000"),
        ("1e15", "1e15", "1e15"),
        ("1e-15", "0.000000000000001", "0.000000000000001"),
        ("1e-16", "1e-16", "1e-16"),
    ] {
        e.execute("DELETE FROM r").expect("delete");
        e.execute(&format!("INSERT INTO r VALUES ({input}, {input})"))
            .expect("insert");
        assert_eq!(text(&mut e, "SELECT x FROM r"), want_float, "float {input}");
        assert_eq!(
            text(&mut e, "SELECT d FROM r"),
            want_double,
            "double {input}"
        );
    }
}

/// The control: a PostgreSQL session keeps PostgreSQL's answers, which
/// differ on every count — FLOAT is eight bytes there, REAL is four, and
/// both print the shortest round-trip in PostgreSQL's own window.
#[test]
fn a_postgres_session_is_untouched() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE p (f FLOAT, r REAL)").expect("ddl");
    e.execute("INSERT INTO p VALUES (3.14159265358979, 3.14159265358979)")
        .expect("insert");
    // PostgreSQL's own spelling for the cast: `CAST(x AS CHAR)` is
    // `char(1)` there and truncates to "3".
    let pg = |e: &mut Engine, col: &str| match one(e, &format!("SELECT {col}::text FROM p")) {
        Value::Text(t) => t.to_string(),
        other => panic!("{other:?}"),
    };
    assert_eq!(pg(&mut e, "f"), "3.14159265358979");
    assert_eq!(pg(&mut e, "r"), "3.1415927");
    // And `DOUBLE(10,2)` is not PostgreSQL syntax.
    assert!(e.execute("CREATE TABLE q (a DOUBLE(10,2))").is_err());
}
