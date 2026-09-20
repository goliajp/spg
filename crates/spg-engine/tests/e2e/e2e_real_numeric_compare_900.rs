//! 9.0.0 — a `real` compared to a `numeric` answered "operator does not
//! exist".
//!
//! Measured against PostgreSQL 18.6 on a `real` column `r`:
//!
//! ```text
//!   WHERE abs(r) < 0.5           PG: the row   SPG: operator does not exist: real < numeric
//!   WHERE (r + 0.0::real) < 0.5  PG: the row   SPG: the same refusal
//!   WHERE 0.5 > abs(r)           PG: the row   SPG: numeric > real
//! ```
//!
//! The numeric comparison arm excluded `Float` from its guard and not
//! `Real`, so the pair entered an arm that cannot widen a float, and the
//! float arm one line below — whose own guard already names both types —
//! never saw it.
//!
//! It reached daylight through pg_trgm: `WHERE (t <-> 'x') < 0.5` is a
//! real against a decimal literal.

use spg_engine::{Engine, QueryResult};

fn col(e: &mut Engine, sql: &str) -> Vec<String> {
    let QueryResult::Rows { rows, .. } = e
        .execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"))
    else {
        panic!("expected rows for {sql}");
    };
    rows.into_iter()
        .map(|r| spg_engine::eval::value_to_text(r.values.first().expect("one column")))
        .collect()
}

fn seeded() -> Engine {
    let mut e = Engine::new();
    for sql in [
        "CREATE TABLE rn (id int, r real, d float8, n numeric)",
        "INSERT INTO rn VALUES (1, 0.25, 0.25, 0.25), (2, 0.75, 0.75, 0.75)",
    ] {
        e.execute(sql)
            .unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    }
    e
}

#[test]
fn a_real_expression_compares_against_a_decimal_literal() {
    let mut e = seeded();
    for sql in [
        "SELECT id FROM rn WHERE abs(r) < 0.5",
        "SELECT id FROM rn WHERE (r + 0.0::real) < 0.5",
        "SELECT id FROM rn WHERE (r * 2.0::real) < 1.0",
        "SELECT id FROM rn WHERE 0.5 > abs(r)",
        "SELECT id FROM rn WHERE r < 0.5",
    ] {
        assert_eq!(col(&mut e, sql), vec!["1".to_string()], "{sql}");
    }
    // A real against a numeric EXPRESSION, both rows qualifying —
    // measured on PG 18.6, which answers 1 and 2 (0.25 < 0.5 and
    // 0.75 < 1.0). The first draft of this pin asserted one row and the
    // differential said otherwise.
    assert_eq!(
        col(&mut e, "SELECT id FROM rn WHERE r < n + 0.25"),
        vec!["1".to_string(), "2".to_string()]
    );
}

#[test]
fn the_select_list_and_the_predicate_agree() {
    // The select list answered all along; the two paths must not differ.
    let mut e = seeded();
    assert_eq!(
        col(&mut e, "SELECT (abs(r) < 0.5)::text FROM rn ORDER BY id"),
        vec!["true".to_string(), "false".to_string()]
    );
}

#[test]
fn float8_against_numeric_still_answers() {
    // The arm this fix widened is the one that serves numeric; the float
    // pairing it already had must keep working.
    let mut e = seeded();
    assert_eq!(
        col(&mut e, "SELECT id FROM rn WHERE abs(d) < 0.5"),
        vec!["1".to_string()]
    );
    assert_eq!(
        col(&mut e, "SELECT id FROM rn WHERE n < 0.5"),
        vec!["1".to_string()]
    );
}
