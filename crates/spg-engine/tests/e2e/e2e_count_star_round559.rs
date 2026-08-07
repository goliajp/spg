//! v7.39 (round 559) — `SELECT count(*)` without touching a row.
//!
//! Phase 5 of the audit is "proactive supremacy", and its rule is that
//! an entry must be able to say why PG cannot do the same — otherwise
//! it is just closing a差分. B1 is exact O(1) count(*). Measuring it
//! first found something the ledger never recorded: over pgwire on
//! 500k rows,
//!
//!     PG18, two parallel workers    8.2 ms
//!     PG18, parallelism off        10.3 ms
//!     SPG                          16.5 ms   = 33 ns/row
//!
//! SPG was 1.6× slower than a SINGLE-THREADED PG on the commonest
//! aggregate there is. Parallelism accounted for only a fifth of PG's
//! lead.
//!
//! The aggregate layer already short-circuits this shape to
//! `rows.len()`, so the O(1) part was never the problem — the cost is
//! UPSTREAM, materialising every visible row so that layer can take its
//! length. Counting visible HEADERS needs no row at all: 16.5 → 4.0 ms,
//! which is also twice as fast as PG's parallel plan.
//!
//! Why PG cannot: its visibility lives in the heap tuples themselves,
//! so it has to read them — that is why `count(*)` is a full scan there,
//! parallel or not. SPG keeps a header array beside the rows, so
//! visibility is answerable without the row.
//!
//! The shape is deliberately narrow — a bare `count(*)` over one plain
//! table, no WHERE, no GROUP BY, no join, no RLS. Anything else keeps
//! the ordinary path, and the pins below say so.

use spg_engine::{Engine, QueryResult, TxId};

const IMPLICIT: TxId = TxId(0);

fn rows(e: &mut Engine, sql: &str) -> Vec<String> {
    match e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err}")) {
        QueryResult::Rows { rows, .. } => rows
            .iter()
            .map(|r| {
                r.values
                    .iter()
                    .map(spg_engine::eval::value_to_text)
                    .collect::<Vec<_>>()
                    .join("|")
            })
            .collect(),
        other => panic!("{sql}: {other:?}"),
    }
}

fn engine() -> Engine {
    let mut e = Engine::new();
    e.execute("CREATE TABLE c (id INT, k INT)").unwrap();
    e.execute("INSERT INTO c SELECT g, g % 10 FROM generate_series(1, 100) g")
        .unwrap();
    e
}

/// It counts, and it keeps counting through every kind of change.
#[test]
fn round559_count_tracks_the_table() {
    let mut e = engine();
    assert_eq!(rows(&mut e, "SELECT count(*) FROM c"), vec!["100"]);
    e.execute("INSERT INTO c VALUES (101, 1)").unwrap();
    assert_eq!(rows(&mut e, "SELECT count(*) FROM c"), vec!["101"]);
    e.execute("DELETE FROM c WHERE id <= 10").unwrap();
    assert_eq!(rows(&mut e, "SELECT count(*) FROM c"), vec!["91"]);
    // An UPDATE appends a new row version under in-place MVCC and
    // tombstones the old one — the count must not double.
    e.execute("UPDATE c SET k = k + 1 WHERE id <= 30").unwrap();
    assert_eq!(rows(&mut e, "SELECT count(*) FROM c"), vec!["91"]);
    e.execute("DELETE FROM c").unwrap();
    assert_eq!(rows(&mut e, "SELECT count(*) FROM c"), vec!["0"]);
}

/// The alias is PG's, and the column name it reports too.
#[test]
fn round559_shape_matches_the_ordinary_path() {
    let mut e = engine();
    match e.execute("SELECT count(*) AS n FROM c").unwrap() {
        QueryResult::Rows { columns, rows } => {
            assert_eq!(columns.len(), 1);
            assert_eq!(columns[0].name, "n");
            assert_eq!(spg_engine::eval::value_to_text(&rows[0].values[0]), "100");
        }
        other => panic!("{other:?}"),
    }
    match e.execute("SELECT count(*) FROM c").unwrap() {
        QueryResult::Rows { columns, .. } => assert_eq!(columns[0].name, "count"),
        other => panic!("{other:?}"),
    }
}

/// A transaction sees its OWN uncommitted writes, and another does not.
#[test]
fn round559_mvcc_visibility_is_respected() {
    let mut e = engine();
    let (t1, t2) = (TxId(71), TxId(72));
    e.execute_in("BEGIN", t1).unwrap();
    e.execute_in("INSERT INTO c VALUES (200, 0)", t1).unwrap();
    // Its own write counts for itself…
    assert_eq!(
        match e.execute_in("SELECT count(*) FROM c", t1).unwrap() {
            QueryResult::Rows { rows, .. } => spg_engine::eval::value_to_text(&rows[0].values[0]),
            other => panic!("{other:?}"),
        },
        "101"
    );
    // …and not for anybody else.
    e.execute_in("BEGIN", t2).unwrap();
    assert_eq!(
        match e.execute_in("SELECT count(*) FROM c", t2).unwrap() {
            QueryResult::Rows { rows, .. } => spg_engine::eval::value_to_text(&rows[0].values[0]),
            other => panic!("{other:?}"),
        },
        "100"
    );
    e.execute_in("COMMIT", t1).unwrap();
    e.execute_in("COMMIT", t2).unwrap();
    assert_eq!(rows(&mut e, "SELECT count(*) FROM c"), vec!["101"]);
}

/// A REPEATABLE READ transaction keeps its frozen count.
#[test]
fn round559_a_frozen_view_keeps_its_count() {
    let mut e = engine();
    let (t1, t2) = (TxId(81), TxId(82));
    e.execute_in("BEGIN ISOLATION LEVEL REPEATABLE READ", t1)
        .unwrap();
    e.execute_in("SELECT count(*) FROM c", t1).unwrap();
    e.execute_in("BEGIN", t2).unwrap();
    e.execute_in("INSERT INTO c VALUES (300, 0)", t2).unwrap();
    e.execute_in("COMMIT", t2).unwrap();
    assert_eq!(
        match e.execute_in("SELECT count(*) FROM c", t1).unwrap() {
            QueryResult::Rows { rows, .. } => spg_engine::eval::value_to_text(&rows[0].values[0]),
            other => panic!("{other:?}"),
        },
        "100",
        "a frozen view must not see a commit that landed after it began"
    );
    e.execute_in("COMMIT", t1).unwrap();
    assert_eq!(rows(&mut e, "SELECT count(*) FROM c"), vec!["101"]);
}

/// Everything that is NOT the bare shape keeps the ordinary path.
#[test]
fn round559_other_shapes_are_untouched() {
    let mut e = engine();
    assert_eq!(
        rows(&mut e, "SELECT count(*) FROM c WHERE k = 1"),
        vec!["10"]
    );
    assert_eq!(
        rows(
            &mut e,
            "SELECT count(*) FROM c GROUP BY k ORDER BY k LIMIT 1"
        ),
        vec!["10"]
    );
    assert_eq!(rows(&mut e, "SELECT count(DISTINCT k) FROM c"), vec!["10"]);
    assert_eq!(rows(&mut e, "SELECT count(id) FROM c"), vec!["100"]);
    assert_eq!(
        rows(&mut e, "SELECT count(*), sum(k) FROM c"),
        vec!["100|450"]
    );
    // A join, a subquery source, and a catalog all keep the old route.
    assert_eq!(
        rows(
            &mut e,
            "SELECT count(*) FROM (SELECT id FROM c WHERE id <= 5) s"
        ),
        vec!["5"]
    );
    assert!(!rows(&mut e, "SELECT count(*) FROM pg_class").is_empty());
}

/// A view over the table still counts through the view.
#[test]
fn round559_a_view_still_counts() {
    let mut e = engine();
    e.execute("CREATE VIEW cv AS SELECT id FROM c WHERE id <= 7")
        .unwrap();
    assert_eq!(rows(&mut e, "SELECT count(*) FROM cv"), vec!["7"]);
}
