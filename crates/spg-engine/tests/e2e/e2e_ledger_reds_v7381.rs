//! 7.38.1 S0.1 — the ledger's red pins, checked in AS reds.
//!
//! Each test here reproduces one open ledger item and asserts the
//! CORRECT (PG) behaviour, so it fails today by design. They ship
//! `#[ignore]`d with the ledger coordinate in the reason string; the
//! fixing section removes the attribute and the pin joins the wall.
//! (r1038 discipline: a pinned defect is a fact, not a promise.)

use spg_engine::{Engine, IMPLICIT_TX, QueryResult};

fn one_cell(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err}")) {
        QueryResult::Rows { rows, .. } => spg_engine::eval::value_to_text(&rows[0].values[0]),
        other => panic!("{sql}: {other:?}"),
    }
}

/// L2 (MATRIX #19) — a neighbour's CREATE SEQUENCE while a poisoned
/// transaction is open must survive that transaction's COMMIT. The
/// shadow-based merge carries tables across; sequence EXISTENCE lives
/// outside the table map and is overwritten today.
#[test]
#[ignore = "7.38.1 L2 red (MATRIX #19) — un-ignore in S3.1"]
fn l2_neighbour_sequence_survives_poisoned_commit() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE base (a INT)").unwrap();
    let tx = e.alloc_tx_id();
    e.execute_in("BEGIN", tx).unwrap();
    e.execute_in("INSERT INTO base VALUES (1)", tx).unwrap();
    // Poison the rebase: DDL inside the transaction.
    e.execute_in("CREATE INDEX base_a ON base (a)", tx).unwrap();
    // Neighbour commits a sequence while the tx is open.
    e.execute_in("CREATE SEQUENCE conc_seq", IMPLICIT_TX)
        .unwrap();
    e.execute_in("COMMIT", tx).unwrap();
    assert_eq!(
        one_cell(&mut e, "SELECT nextval('conc_seq')"),
        "1",
        "the neighbour's sequence must survive the poisoned COMMIT"
    );
}

/// L2 sibling — a neighbour's VIEW, same window.
#[test]
#[ignore = "7.38.1 L2 red (MATRIX #19) — un-ignore in S3.1"]
fn l2_neighbour_view_survives_poisoned_commit() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE base2 (a INT)").unwrap();
    e.execute("CREATE TABLE seen (x INT)").unwrap();
    e.execute("INSERT INTO seen VALUES (7)").unwrap();
    let tx = e.alloc_tx_id();
    e.execute_in("BEGIN", tx).unwrap();
    e.execute_in("INSERT INTO base2 VALUES (1)", tx).unwrap();
    e.execute_in("CREATE INDEX base2_a ON base2 (a)", tx)
        .unwrap();
    e.execute_in("CREATE VIEW conc_view AS SELECT x FROM seen", IMPLICIT_TX)
        .unwrap();
    e.execute_in("COMMIT", tx).unwrap();
    assert_eq!(
        one_cell(&mut e, "SELECT count(*) FROM conc_view"),
        "1",
        "the neighbour's view must survive the poisoned COMMIT"
    );
}

/// L4 (MATRIX #18) — embedded text for timestamptz must carry the PG
/// offset suffix, exactly what the wire already sends (live PG18:
/// `2026-01-05 09:00:00+00`). Today `value_to_text` drops it.
#[test]
#[ignore = "7.38.1 L4 red (MATRIX #18) — un-ignore in S4.1"]
fn l4_timestamptz_text_carries_the_pg_offset() {
    let mut e = Engine::new().with_clock(|| 0);
    assert_eq!(
        one_cell(&mut e, "SELECT '2026-01-05 09:00:00+00'::timestamptz"),
        "2026-01-05 09:00:00+00",
    );
}
