//! 9.0.0 — `oid` compared equal to a numeric or a real.
//!
//! Measured on PostgreSQL 18.6:
//!
//! ```text
//!   1::oid = 1           t          1::oid = 1::numeric   operator does not exist
//!   1::oid = 1::bigint   t          1::oid = 1::real      operator does not exist
//!                                   1::oid = 1.0          operator does not exist: oid = numeric
//! ```
//!
//! SPG answered `t` to all six: a value cast to `oid` is carried as a
//! plain integer, so the comparison saw two integers. The rule was
//! already written down -- `types_unify` puts oid with the integer
//! widths and the reg types and with nothing else, and the set
//! operations, `IN` lists and `CASE` arms consult it. A bare comparison
//! did not.
//!
//! The check fires only when both operands have a type certain without a
//! row and one of them is oid-ish, so an untyped literal is untouched.

use spg_engine::{Engine, QueryResult};

fn one(e: &mut Engine, sql: &str) -> String {
    let QueryResult::Rows { rows, .. } = e
        .execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"))
    else {
        panic!("expected rows for {sql}");
    };
    spg_engine::eval::value_to_text(
        rows.first()
            .expect("one row")
            .values
            .first()
            .expect("one column"),
    )
}

#[test]
fn oid_against_a_non_integer_is_refused() {
    let mut e = Engine::new();
    for (sql, want) in [
        ("SELECT 1::oid = 1::numeric", "oid = numeric"),
        ("SELECT 1::oid = 1::real", "oid = real"),
        ("SELECT 1::oid = 1.0", "oid = numeric"),
        ("SELECT 1::oid <> 1::numeric", "oid <> numeric"),
        ("SELECT 1::oid < 1::numeric", "oid < numeric"),
    ] {
        let err = e.execute(sql).expect_err(sql);
        let msg = format!("{err:?}");
        assert!(
            msg.contains(&format!("operator does not exist: {want}")),
            "{sql}: {msg}"
        );
    }
}

#[test]
fn oid_against_an_integer_width_still_answers() {
    let mut e = Engine::new();
    assert_eq!(one(&mut e, "SELECT (1::oid = 1)::text"), "true");
    assert_eq!(one(&mut e, "SELECT (1::oid = 1::bigint)::text"), "true");
    assert_eq!(one(&mut e, "SELECT (1::oid = 1::smallint)::text"), "true");
    assert_eq!(one(&mut e, "SELECT (1::oid = 1::oid)::text"), "true");
}

#[test]
fn an_untyped_literal_is_not_judged() {
    // The over-refusal a wider version of this check was backed out for:
    // PostgreSQL coerces an unknown literal to the other side's type.
    let mut e = Engine::new();
    e.execute("CREATE TABLE ot (i int, t text)")
        .expect("create");
    e.execute("INSERT INTO ot VALUES (5, 'five')").expect("ins");
    assert_eq!(one(&mut e, "SELECT t FROM ot WHERE i = '5'"), "five");
    // And a catalog query that compares an oid column to a bare integer
    // -- how every client names a type -- keeps answering.
    assert_eq!(
        one(
            &mut e,
            "SELECT count(*)::text FROM pg_class WHERE oid = 1259"
        ),
        "1"
    );
}
