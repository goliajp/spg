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

/// S3.1 breadth — the remaining D4 families (enum, domain, composite
/// type, materialized view), same window: a neighbour's CREATE while
/// a poisoned tx is open must survive that tx's COMMIT.
#[test]
fn l2_neighbour_type_family_survives_poisoned_commit() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE base3 (a INT)").unwrap();
    e.execute("CREATE TABLE mv_src (x INT)").unwrap();
    e.execute("INSERT INTO mv_src VALUES (5)").unwrap();
    let tx = e.alloc_tx_id();
    e.execute_in("BEGIN", tx).unwrap();
    e.execute_in("INSERT INTO base3 VALUES (1)", tx).unwrap();
    e.execute_in("CREATE INDEX base3_a ON base3 (a)", tx)
        .unwrap();
    e.execute_in("CREATE TYPE conc_mood AS ENUM ('a','b')", IMPLICIT_TX)
        .unwrap();
    e.execute_in(
        "CREATE DOMAIN conc_dom AS INT CHECK (VALUE > 0)",
        IMPLICIT_TX,
    )
    .unwrap();
    e.execute_in("CREATE TYPE conc_pair AS (l INT, r INT)", IMPLICIT_TX)
        .unwrap();
    e.execute_in(
        "CREATE MATERIALIZED VIEW conc_mv AS SELECT x FROM mv_src",
        IMPLICIT_TX,
    )
    .unwrap();
    e.execute_in("COMMIT", tx).unwrap();
    assert_eq!(one_cell(&mut e, "SELECT 'a'::conc_mood"), "a");
    assert_eq!(one_cell(&mut e, "SELECT 3::conc_dom"), "3");
    assert_eq!(one_cell(&mut e, "SELECT (ROW(1,2)::conc_pair).l"), "1");
    assert_eq!(one_cell(&mut e, "SELECT count(*) FROM conc_mv"), "1");
}

/// S3.1 breadth — the drop direction: a neighbour's DROP of a view
/// and a sequence must not be resurrected by the poisoned COMMIT.
#[test]
fn l2_neighbour_drops_stay_dropped_after_poisoned_commit() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE base4 (a INT)").unwrap();
    e.execute("CREATE SEQUENCE dead_seq").unwrap();
    e.execute("CREATE VIEW dead_view AS SELECT a FROM base4")
        .unwrap();
    let tx = e.alloc_tx_id();
    e.execute_in("BEGIN", tx).unwrap();
    e.execute_in("INSERT INTO base4 VALUES (1)", tx).unwrap();
    e.execute_in("CREATE INDEX base4_a ON base4 (a)", tx)
        .unwrap();
    e.execute_in("DROP SEQUENCE dead_seq", IMPLICIT_TX).unwrap();
    e.execute_in("DROP VIEW dead_view", IMPLICIT_TX).unwrap();
    e.execute_in("COMMIT", tx).unwrap();
    assert!(
        e.execute("SELECT nextval('dead_seq')").is_err(),
        "the neighbour's DROP SEQUENCE must survive the poisoned COMMIT"
    );
    assert!(
        e.execute("SELECT * FROM dead_view").is_err(),
        "the neighbour's DROP VIEW must survive the poisoned COMMIT"
    );
}
