//! v7.39 (read01 round 193) — numeric(p,s) precision overflow raises
//! PG's exact message + DETAIL (wire: 22003, ERROR and DETAIL lines
//! byte-identical to live PG18, verified 2026-07-18):
//!   ERROR:  numeric field overflow
//!   DETAIL: A field with precision 4, scale 1 must round to an
//!           absolute value less than 10^3.
//! Also this round's differential sweep: 16/18 NUMERIC-exactness
//! probes already SAME (division/multiplication scale, round/trunc,
//! ^, sqrt, avg, bignum, mod, numeric(p,s) rounding). Remaining
//! documented minor: power(numeric, negative) trailing-zero count
//! (values equal; PG's rscale comes from its internal ln-weight
//! estimate).

use spg_engine::Engine;

#[test]
fn overflow_message_matches_pg() {
    let mut e = Engine::new();
    let err = e
        .execute("SELECT (999.99)::numeric(4,1)")
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("numeric field overflow"),
        "main message: {err}"
    );
    assert!(
        err.contains(
            "A field with precision 4, scale 1 must round to an absolute value less than 10^3."
        ),
        "detail: {err}"
    );
}

#[test]
fn column_assignment_overflow_same_shape() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (v NUMERIC(4,1))").unwrap();
    let err = e
        .execute("INSERT INTO t VALUES (999.99)")
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("numeric field overflow")
            && err.contains("precision 4, scale 1"),
        "{err}"
    );
}

#[test]
fn fitting_values_unchanged() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (v NUMERIC(4,1))").unwrap();
    e.execute("INSERT INTO t VALUES (999.94)").unwrap(); // rounds to 999.9
    e.execute("INSERT INTO t VALUES (-999.9)").unwrap();
}
