//! v7.39.3 — a MySQL `FLOAT(m,d)` / `DOUBLE(m,d)` rounds on write.
//!
//! 7.39.2 accepted the syntax — before it, `DOUBLE(10,2)` failed the
//! whole CREATE — and recorded the rest as a residual: "the digits are
//! not a display hint. MySQL ROUNDS on write (3.14159265358979 into
//! either stores 3.14) and reports `float(10,2)` in COLUMN_TYPE."
//! A column declared for money held more precision than its schema said,
//! and every reader saw a different number from MySQL's.
//!
//! The tie rule is measured, not reasoned:
//!
//!   d >= 1   round-half-to-EVEN on the value's true binary magnitude.
//!            Eleven exactly-representable ties agree: 0.25 -> 0.2,
//!            0.75 -> 0.8, 1.25 -> 1.2, 1.75 -> 1.8, and the negatives.
//!   d = 0    ties go toward NEGATIVE INFINITY instead: 0.5 -> 0,
//!            1.5 -> 1, 2.5 -> 2, -0.5 -> -1. Non-ties round normally
//!            there (1.6 -> 2). Why MySQL differs between the two is
//!            not explained here; this matches the behaviour.
//!
//! The first implementation scaled by `10^d` and rounded the product,
//! which lands `2.3455 * 1000.0` just ABOVE the tie in f64 and stored
//! 2.346 where MySQL stores 2.345 — the value itself is just below it.
//! Formatting to `d` places rounds the true magnitude, which is the
//! rule that was measured.

use spg_engine::{Engine, QueryResult};

fn mysql() -> Engine {
    let mut e = Engine::new();
    e.set_mysql_dialect(true);
    e
}

fn one(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql) {
        Ok(QueryResult::Rows { rows, .. }) if !rows.is_empty() => {
            spg_engine::eval::value_to_text(&rows[0].values[0])
        }
        Ok(other) => panic!("{sql}: {other:?}"),
        Err(err) => panic!("{sql}: {err}"),
    }
}

#[test]
fn the_declared_pair_rounds_on_write() {
    let mut e = mysql();
    e.execute("CREATE TABLE md (a FLOAT(10,2), b DOUBLE(10,2), c DOUBLE(6,3))")
        .expect("ddl");
    e.execute(
        "INSERT INTO md VALUES (3.14159265358979, 3.14159265358979, 3.14159265358979), \
         (2.345, 2.345, 2.3455)",
    )
    .expect("insert");
    // Measured on MySQL 9.7.2 for exactly these inserts.
    assert_eq!(one(&mut e, "SELECT a FROM md ORDER BY a LIMIT 1"), "2.35");
    assert_eq!(one(&mut e, "SELECT b FROM md ORDER BY b LIMIT 1"), "2.35");
    // The one the first implementation got wrong: 2.3455 is just BELOW
    // the tie, so it goes down.
    assert_eq!(one(&mut e, "SELECT c FROM md ORDER BY c LIMIT 1"), "2.345");
    assert_eq!(
        one(&mut e, "SELECT a FROM md ORDER BY a DESC LIMIT 1"),
        "3.14"
    );
}

#[test]
fn the_tie_rule_is_the_measured_one() {
    let mut e = mysql();
    e.execute("CREATE TABLE t1 (v DOUBLE(10,1))").expect("ddl");
    for (input, want) in [
        ("0.25", "0.2"),
        ("0.75", "0.8"),
        ("1.25", "1.2"),
        ("1.75", "1.8"),
        ("-0.25", "-0.2"),
        ("-0.75", "-0.8"),
    ] {
        e.execute("DELETE FROM t1").expect("delete");
        e.execute(&format!("INSERT INTO t1 VALUES ({input})"))
            .expect("insert");
        assert_eq!(one(&mut e, "SELECT v FROM t1"), want, "d=1 tie {input}");
    }
    // d = 0 is a different rule — ties toward negative infinity.
    e.execute("CREATE TABLE t0 (v DOUBLE(10,0))").expect("ddl");
    for (input, want) in [
        ("0.5", "0"),
        ("1.5", "1"),
        ("2.5", "2"),
        ("-0.5", "-1"),
        // and a non-tie rounds normally there
        ("1.6", "2"),
        ("1.4", "1"),
    ] {
        e.execute("DELETE FROM t0").expect("delete");
        e.execute(&format!("INSERT INTO t0 VALUES ({input})"))
            .expect("insert");
        assert_eq!(one(&mut e, "SELECT v FROM t0"), want, "d=0 {input}");
    }
}

#[test]
fn a_value_wider_than_m_is_refused() {
    let mut e = mysql();
    e.execute("CREATE TABLE ovf (a DOUBLE(10,2))").expect("ddl");
    let err = format!(
        "{}",
        e.execute("INSERT INTO ovf VALUES (99999999999)")
            .unwrap_err()
    );
    assert!(
        err.contains("Out of range value for column 'a'"),
        "wanted MySQL's out-of-range wording, said {err:?}"
    );
    // The control: a value that fits still lands.
    e.execute("INSERT INTO ovf VALUES (12345678.99)")
        .expect("insert");
    assert_eq!(one(&mut e, "SELECT a FROM ovf"), "12345678.99");
}

#[test]
fn the_pair_is_part_of_the_type_a_client_reads() {
    let mut e = mysql();
    e.execute("CREATE TABLE ty (a FLOAT(10,2), b DOUBLE(6,3), c DOUBLE)")
        .expect("ddl");
    for (col, want_type, want_prec, want_scale) in [
        ("a", "float(10,2)", "10", "2"),
        ("b", "double(6,3)", "6", "3"),
        // No declared pair: unchanged.
        ("c", "double", "22", "NULL"),
    ] {
        let q = |f: &str| {
            format!(
                "SELECT {f} FROM information_schema.columns \
                 WHERE table_name = 'ty' AND column_name = '{col}'"
            )
        };
        assert_eq!(one(&mut e, &q("column_type")), want_type, "{col} type");
        assert_eq!(
            one(&mut e, &q("numeric_precision")),
            want_prec,
            "{col} precision"
        );
        assert_eq!(one(&mut e, &q("numeric_scale")), want_scale, "{col} scale");
    }
}

/// The control: a PostgreSQL session has no such syntax, so nothing it
/// declares carries a pair and nothing it writes is rounded.
#[test]
fn a_postgres_session_is_untouched() {
    let mut e = Engine::new();
    assert!(e.execute("CREATE TABLE p (a DOUBLE(10,2))").is_err());
    e.execute("CREATE TABLE q (a FLOAT)").expect("ddl");
    e.execute("INSERT INTO q VALUES (3.14159265358979)")
        .expect("insert");
    assert_eq!(one(&mut e, "SELECT a::text FROM q"), "3.14159265358979");
}
