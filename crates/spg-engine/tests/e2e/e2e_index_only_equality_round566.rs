//! v7.39 (round 566) — equality is a range with both ends the same.
//!
//! Round 560 built the index-only scan for two-sided ranges and left
//! equality out, which is the commoner query. Measured on 500k rows with
//! 50 distinct values, so `WHERE g = 7` returns 10k of them — over
//! pgwire, three paired batches of four, interleaved:
//!
//!     before  9.5 ms        after  3.14 ms        PG18  1.49 ms
//!
//! -67%, and the ratio goes 6.4x -> 2.1x. PG still leads: this closes
//! most of the loss, not the loss.
//!
//! One reading in this round was taken against a stale PG number and
//! said SPG had gone ahead. PG's plan had switched to `Index Only Scan`
//! between the two samples, so the pair was not comparable; the
//! interleaved run above is the one that counts. Same defect round 561
//! found in round 560's record, one round after it was written down.
//!
//! The enum guard on ranges does not reach equality, deliberately. It
//! exists because the index orders labels lexicographically while PG
//! orders them by catalog position, so a range walk would under-select
//! — equality does not depend on the order, which is what
//! `try_index_seek_positions` already says about its own seek.
//!
//! `col = NULL` is never true, so it is refused before it can ask an
//! index that stores NULL keys for them.

use spg_engine::{Engine, QueryResult, TxId};

const IMPLICIT: TxId = TxId(0);

fn vals(e: &mut Engine, sql: &str) -> Vec<String> {
    match e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err}")) {
        QueryResult::Rows { rows, .. } => rows
            .iter()
            .map(|r| spg_engine::eval::value_to_text(&r.values[0]))
            .collect(),
        other => panic!("{sql}: {other:?}"),
    }
}

fn engine() -> Engine {
    let mut e = Engine::new();
    e.execute("CREATE TABLE q566 (id INT, g INT, t TEXT)")
        .unwrap();
    e.execute("INSERT INTO q566 SELECT gg, gg % 10, 'x' FROM generate_series(1, 500) gg")
        .unwrap();
    e.execute("CREATE INDEX q566g ON q566 (g)").unwrap();
    e
}

/// Every duplicate comes back, once each, and the ordinary path agrees.
#[test]
fn round566_equality_returns_every_duplicate() {
    let mut e = engine();
    let got = vals(&mut e, "SELECT g FROM q566 WHERE g = 7");
    assert_eq!(got.len(), 50, "500 rows, 10 distinct values");
    assert!(got.iter().all(|v| v == "7"), "{got:?}");
    // The row-fetching path on the same predicate.
    assert_eq!(vals(&mut e, "SELECT id FROM q566 WHERE g = 7").len(), 50);
    // A value that is not there.
    assert!(vals(&mut e, "SELECT g FROM q566 WHERE g = 99").is_empty());
    // Written the other way round.
    assert_eq!(vals(&mut e, "SELECT g FROM q566 WHERE 7 = g").len(), 50);
    // And with the column qualified, and the output aliased.
    assert_eq!(
        vals(&mut e, "SELECT q.g FROM q566 q WHERE q.g = 7").len(),
        50
    );
    assert_eq!(
        vals(&mut e, "SELECT g AS n FROM q566 WHERE g = 7").len(),
        50
    );
}

/// `col = NULL` is never true — an index that stores NULL keys must not
/// be asked for them.
#[test]
fn round566_equality_to_null_matches_nothing() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE n566 (id INT, g INT)").unwrap();
    e.execute("INSERT INTO n566 VALUES (1, NULL), (2, NULL), (3, 5)")
        .unwrap();
    e.execute("CREATE INDEX n566g ON n566 (g)").unwrap();
    assert!(vals(&mut e, "SELECT g FROM n566 WHERE g = NULL").is_empty());
    assert_eq!(vals(&mut e, "SELECT g FROM n566 WHERE g IS NULL").len(), 2);
    assert_eq!(vals(&mut e, "SELECT g FROM n566 WHERE g = 5"), vec!["5"]);
}

/// Deleted and updated versions stay out, and a transaction sees only
/// its own writes.
#[test]
fn round566_visibility_holds_for_equality() {
    let mut e = engine();
    e.execute("DELETE FROM q566 WHERE g = 7 AND id <= 200")
        .unwrap();
    assert_eq!(vals(&mut e, "SELECT g FROM q566 WHERE g = 7").len(), 30);
    // An UPDATE tombstones the old version and appends a new one.
    e.execute("UPDATE q566 SET g = 7 WHERE g = 3").unwrap();
    assert_eq!(vals(&mut e, "SELECT g FROM q566 WHERE g = 7").len(), 80);
    assert!(vals(&mut e, "SELECT g FROM q566 WHERE g = 3").is_empty());

    let (t1, t2) = (TxId(41), TxId(42));
    e.execute_in("BEGIN", t1).unwrap();
    e.execute_in("INSERT INTO q566 VALUES (9001, 7, 'z')", t1)
        .unwrap();
    let seen = |e: &mut Engine, tx: TxId| match e
        .execute_in("SELECT g FROM q566 WHERE g = 7", tx)
        .unwrap()
    {
        QueryResult::Rows { rows, .. } => rows.len(),
        other => panic!("{other:?}"),
    };
    assert_eq!(seen(&mut e, t1), 81, "its own insert");
    e.execute_in("BEGIN", t2).unwrap();
    assert_eq!(seen(&mut e, t2), 80, "not another transaction's");
    e.execute_in("COMMIT", t1).unwrap();
    e.execute_in("COMMIT", t2).unwrap();
    assert_eq!(vals(&mut e, "SELECT g FROM q566 WHERE g = 7").len(), 81);
}

/// The types keep their own rules — a key that does not restore its
/// column still takes the ordinary path, and answers the same.
#[test]
fn round566_types_follow_the_same_rule() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t566 (d DATE, u UUID, b BOOL, s TEXT)")
        .unwrap();
    e.execute(
        "INSERT INTO t566 VALUES \
         ('2026-01-01','11111111-1111-1111-1111-111111111111',true,'a'), \
         ('2026-01-02','22222222-2222-2222-2222-222222222222',false,'b')",
    )
    .unwrap();
    for ddl in [
        "CREATE INDEX t566d ON t566 (d)",
        "CREATE INDEX t566u ON t566 (u)",
        "CREATE INDEX t566b ON t566 (b)",
        "CREATE INDEX t566s ON t566 (s)",
    ] {
        e.execute(ddl).unwrap();
    }
    // A date does not come back from its key, so this reads the row —
    // and must still be a date.
    assert_eq!(
        vals(&mut e, "SELECT d FROM t566 WHERE d = '2026-01-02'"),
        vec!["2026-01-02"]
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT u FROM t566 WHERE u = '22222222-2222-2222-2222-222222222222'"
        ),
        vec!["22222222-2222-2222-2222-222222222222"]
    );
    assert_eq!(
        vals(&mut e, "SELECT b FROM t566 WHERE b = true"),
        vec!["true"]
    );
    assert_eq!(vals(&mut e, "SELECT s FROM t566 WHERE s = 'b'"), vec!["b"]);
    assert_eq!(
        vals(&mut e, "SELECT pg_typeof(s) FROM t566 WHERE s = 'b'"),
        vec!["text"]
    );

    // The strongest form of the question round 564 asked: drop the
    // index and the answers must not move.
    let with_index: Vec<Vec<String>> = [
        "SELECT d FROM t566 WHERE d = '2026-01-02'",
        "SELECT u FROM t566 WHERE u = '22222222-2222-2222-2222-222222222222'",
        "SELECT b FROM t566 WHERE b = true",
        "SELECT s FROM t566 WHERE s = 'b'",
        "SELECT pg_typeof(s) FROM t566 WHERE s = 'b'",
    ]
    .iter()
    .map(|q| vals(&mut e, q))
    .collect();
    for ddl in [
        "DROP INDEX t566d",
        "DROP INDEX t566u",
        "DROP INDEX t566b",
        "DROP INDEX t566s",
    ] {
        e.execute(ddl).unwrap();
    }
    let without_index: Vec<Vec<String>> = [
        "SELECT d FROM t566 WHERE d = '2026-01-02'",
        "SELECT u FROM t566 WHERE u = '22222222-2222-2222-2222-222222222222'",
        "SELECT b FROM t566 WHERE b = true",
        "SELECT s FROM t566 WHERE s = 'b'",
        "SELECT pg_typeof(s) FROM t566 WHERE s = 'b'",
    ]
    .iter()
    .map(|q| vals(&mut e, q))
    .collect();
    assert_eq!(
        with_index, without_index,
        "an index may not change an answer"
    );
}

/// An enum column keeps its label — equality does not depend on the
/// index's lexicographic order the way a range does.
#[test]
fn round566_enum_equality_is_exact() {
    let mut e = Engine::new();
    e.execute("CREATE TYPE mood566 AS ENUM ('sad', 'ok', 'happy')")
        .unwrap();
    e.execute("CREATE TABLE m566 (id INT, m mood566)").unwrap();
    e.execute("INSERT INTO m566 VALUES (1,'sad'),(2,'happy'),(3,'ok'),(4,'happy')")
        .unwrap();
    e.execute("CREATE INDEX m566m ON m566 (m)").unwrap();
    let got = vals(&mut e, "SELECT m FROM m566 WHERE m = 'happy'");
    assert_eq!(got.len(), 2, "{got:?}");
    assert!(got.iter().all(|v| v == "happy"), "{got:?}");
    // The catalog order, not the lexicographic one, still drives ORDER BY.
    assert_eq!(
        vals(&mut e, "SELECT m FROM m566 ORDER BY m"),
        vec!["sad", "ok", "happy", "happy"]
    );
}
