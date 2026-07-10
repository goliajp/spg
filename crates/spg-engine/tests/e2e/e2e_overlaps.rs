//! SQL-standard (S1, E1) OVERLAPS (S2, E2) — least/greatest
//! normalisation + start1 < end2 AND start2 < end1 lowering.

use spg_engine::{Engine, QueryResult};

fn survives(e: &mut Engine, cond: &str) -> bool {
    let sql = format!("SELECT 1 WHERE {cond}");
    let r = e
        .execute(&sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    let QueryResult::Rows { rows, .. } = r else {
        panic!("expected Rows");
    };
    !rows.is_empty()
}

#[test]
fn date_periods() {
    let mut e = Engine::new();
    assert!(survives(
        &mut e,
        "(DATE '2024-01-01', DATE '2024-06-01') OVERLAPS (DATE '2024-03-01', DATE '2024-09-01')"
    ));
    // Disjoint periods.
    assert!(!survives(
        &mut e,
        "(DATE '2024-01-01', DATE '2024-02-01') OVERLAPS (DATE '2024-03-01', DATE '2024-04-01')"
    ));
    // Touching endpoints do NOT overlap (half-open periods, PG doc
    // example: shared single point only at the boundary).
    assert!(!survives(
        &mut e,
        "(DATE '2024-01-01', DATE '2024-03-01') OVERLAPS (DATE '2024-03-01', DATE '2024-05-01')"
    ));
}

#[test]
fn endpoint_order_and_row_keyword() {
    let mut e = Engine::new();
    // PG accepts the pair in either order — normalised internally.
    assert!(survives(
        &mut e,
        "(DATE '2024-06-01', DATE '2024-01-01') OVERLAPS (DATE '2024-03-01', DATE '2024-09-01')"
    ));
    // ROW keyword spelling on both sides.
    assert!(survives(
        &mut e,
        "ROW(DATE '2024-01-01', DATE '2024-06-01') OVERLAPS ROW(DATE '2024-03-01', DATE '2024-09-01')"
    ));
    // Plain numbers work too (same lowering).
    assert!(survives(&mut e, "(1, 5) OVERLAPS (4, 9)"));
    assert!(!survives(&mut e, "(1, 4) OVERLAPS (4, 9)"));
    // Arity errors.
    assert!(
        e.execute("SELECT 1 WHERE (1, 2, 3) OVERLAPS (4, 5)")
            .is_err()
    );
}
