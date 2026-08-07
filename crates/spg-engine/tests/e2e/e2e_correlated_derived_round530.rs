//! v7.39 (round 530) — a correlated subquery over a DERIVED TABLE.
//!
//! This round widened the sweep from read shapes to write and DDL ones.
//! `UPDATE … FROM` was the shape that broke, and chasing it landed
//! somewhere much wider than UPDATE:
//!
//!     SELECT id, EXISTS(SELECT 1 FROM (SELECT 1 AS id) x
//!                       WHERE t.id = x.id) FROM t
//!     PG18  1|t 2|f 3|f        SPG  1|t 2|t 3|t
//!
//! `select_is_correlated` answered "not correlated" for ANY subquery
//! whose FROM held a derived table, with the note that its scope was
//! beyond that cheap check. The direction was backwards: an uncorrelated
//! subquery is evaluated ONCE and its answer reused for every outer row,
//! so a wrong "no" is silently wrong, while a wrong "yes" only costs a
//! re-evaluation.
//!
//! It reached plain SELECTs — any `WHERE EXISTS (SELECT … FROM
//! (subquery) … WHERE <correlation>)` matched everything — and through
//! the parser's `UPDATE … FROM` lowering, which builds exactly that
//! EXISTS, it updated every row of the target table.
//!
//! Every expectation below is a PG18 reading.

use spg_engine::{Engine, QueryResult};

fn engine() -> Engine {
    let mut e = Engine::new();
    e.execute("CREATE TABLE a (id INT, v INT)").unwrap();
    e.execute("CREATE TABLE b (id INT, d INT)").unwrap();
    e.execute("INSERT INTO a VALUES (1,10),(2,20),(3,30)")
        .unwrap();
    e.execute("INSERT INTO b VALUES (1,100),(3,300)").unwrap();
    e
}

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

/// The read shape, which is where this actually lives.
#[test]
fn round530_correlated_exists_over_a_derived_table() {
    let mut e = engine();
    assert_eq!(
        rows(
            &mut e,
            "SELECT id, EXISTS(SELECT 1 FROM (SELECT 1 AS id, 5 AS d) x \
             WHERE a.id = x.id) FROM a ORDER BY id"
        ),
        vec!["1|true", "2|false", "3|false"]
    );
    // A correlated subquery over a real TABLE always worked; it still does.
    assert_eq!(
        rows(
            &mut e,
            "SELECT id, EXISTS(SELECT 1 FROM b WHERE a.id = b.id) FROM a ORDER BY id"
        ),
        vec!["1|true", "2|false", "3|true"]
    );
}

/// The scalar form answered the same value for every row.
#[test]
fn round530_correlated_scalar_over_a_derived_table() {
    let mut e = engine();
    assert_eq!(
        rows(
            &mut e,
            "SELECT id, (SELECT x.d FROM (SELECT 1 AS id, 5 AS d) x \
             WHERE a.id = x.id) FROM a ORDER BY id"
        ),
        vec!["1|5", "2|NULL", "3|NULL"]
    );
}

/// And the write shape that led here: the parser lowers `UPDATE … FROM`
/// onto that EXISTS, so every target row matched.
#[test]
fn round530_update_from_a_derived_table_touches_only_matches() {
    let mut e = engine();
    e.execute("UPDATE a SET v = 0").unwrap();
    e.execute("UPDATE a SET v = v + x.d FROM (SELECT 1 AS id, 5 AS d) x WHERE a.id = x.id")
        .unwrap();
    assert_eq!(
        rows(&mut e, "SELECT v FROM a ORDER BY id"),
        vec!["5", "0", "0"]
    );
    // A constant assignment takes the same path.
    e.execute("UPDATE a SET v = 0").unwrap();
    e.execute("UPDATE a SET v = 9 FROM (SELECT 1 AS id) x WHERE a.id = x.id")
        .unwrap();
    assert_eq!(
        rows(&mut e, "SELECT v FROM a ORDER BY id"),
        vec!["9", "0", "0"]
    );
}

/// `UPDATE … FROM <real table>` was right before and is still right —
/// it is the case that made the derived-table one look plausible.
#[test]
fn round530_update_from_a_real_table_unchanged() {
    let mut e = engine();
    e.execute("UPDATE a SET v = b.d FROM b WHERE a.id = b.id")
        .unwrap();
    assert_eq!(
        rows(&mut e, "SELECT v FROM a ORDER BY id"),
        vec!["100", "20", "300"]
    );
    // With a further filter on the source.
    e.execute("UPDATE a SET v = 0").unwrap();
    e.execute("UPDATE a SET v = 7 FROM b WHERE a.id = b.id AND b.d > 200")
        .unwrap();
    assert_eq!(
        rows(&mut e, "SELECT v FROM a ORDER BY id"),
        vec!["0", "0", "7"]
    );
}

/// A one-level LATERAL, and a derived table with more than one row —
/// the correlation has to pick the right row, not just decide whether
/// any exists.
#[test]
fn round530_lateral_and_multi_row_derived_still_correlate() {
    let mut e = engine();
    assert_eq!(
        rows(
            &mut e,
            "SELECT a.id, l.k FROM a, LATERAL (SELECT a.v + 1 AS k) l ORDER BY a.id"
        ),
        vec!["1|11", "2|21", "3|31"]
    );
    assert_eq!(
        rows(
            &mut e,
            "SELECT id, (SELECT count(*) FROM (SELECT 1 AS id UNION ALL SELECT 2) x \
             WHERE x.id = a.id) FROM a ORDER BY id"
        ),
        vec!["1|1", "2|1", "3|0"]
    );
}
