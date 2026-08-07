//! v7.39 (rounds 655-657) — a scalar aggregate's working memory was O(rows).
//!
//! Measured on the running server, RSS before and after the first scan, four
//! sizes:
//!
//! | rows | HEAD | r656 | r657 |
//! |------|------|------|------|
//! | 100k | 72 B/row | 7 | 6 |
//! | 250k | 81 | 16 | 12 |
//! | 500k | 82 | 18 | 14 |
//! | 1M   | 81 | 17 | 15 |
//!
//! `SELECT sum(id) FROM d` returns one number and used to allocate 81 bytes
//! per input row to do it — 3.2 GB at 50M rows, where what the customer meets
//! is not slowness but OOM. Two causes, both measured rather than reasoned:
//!
//! * `run_single_table_aggregate` collected one 64-byte `RowRef` per
//!   surviving row on top of the `Vec<&Row>` it already had. `RowRef` is
//!   that big because its `Tuple` variant carries four slice references for
//!   the join path; a scan only ever uses the 8-byte `Owned`. Fixed by
//!   `AggRows::Ptrs`, which reads the pointers the scan already has.
//! * the survivor vector walked the doubling chain, and every abandoned
//!   buffer stays resident because RSS is a high-water mark. Fixed by
//!   reserving — but ONLY when there is no WHERE, since with one the row
//!   count is an upper bound and reserving it would be the worse trade.
//!
//! These are behaviour-free changes, so what follows pins the behaviour
//! that must not move, plus the shape of the reservation decision.

use spg_engine::{Engine, QueryResult};

fn one(e: &mut Engine, sql: &str) -> String {
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
            .collect::<Vec<_>>()
            .join(","),
        other => panic!("{sql}: {other:?}"),
    }
}

fn seeded() -> Engine {
    let mut e = Engine::new();
    e.execute("CREATE TABLE d (id INT, g INT, s TEXT)").unwrap();
    for i in 1..=200 {
        e.execute(&format!("INSERT INTO d VALUES ({i}, {}, 'row{i}')", i % 10))
            .unwrap();
    }
    e
}

/// The scan path now hands the aggregate its pointers instead of a second
/// vector of wrappers. Every aggregate shape must answer exactly as before.
#[test]
fn round656_aggregates_answer_the_same_through_aggrows() {
    let mut e = seeded();
    assert_eq!(one(&mut e, "SELECT count(*) FROM d"), "200");
    assert_eq!(one(&mut e, "SELECT count(id) FROM d"), "200");
    assert_eq!(one(&mut e, "SELECT sum(id) FROM d"), "20100");
    assert_eq!(one(&mut e, "SELECT min(id), max(id) FROM d"), "1|200");
    assert_eq!(one(&mut e, "SELECT count(DISTINCT g) FROM d"), "10");
    assert_eq!(one(&mut e, "SELECT max(s) FROM d"), "row99");
    assert_eq!(
        one(
            &mut e,
            "SELECT g, count(*) FROM d WHERE g < 2 GROUP BY g ORDER BY g"
        ),
        "0|20,1|20"
    );
    assert_eq!(one(&mut e, "SELECT count(*) FROM d WHERE id > 190"), "10");
    // HAVING, ORDER BY over the aggregate, and an empty result all ride the
    // same input.
    assert_eq!(
        one(
            &mut e,
            "SELECT g, sum(id) FROM d GROUP BY g HAVING sum(id) > 2050 ORDER BY g DESC LIMIT 2"
        ),
        // Values taken from PG18, not from arithmetic in my head — the
        // first version of this line had 2090/2080 and was simply wrong.
        "9|2080,8|2060"
    );
    assert_eq!(one(&mut e, "SELECT count(*) FROM d WHERE id > 1000"), "0");
    assert_eq!(one(&mut e, "SELECT sum(id) FROM d WHERE id > 1000"), "NULL");
}

/// The join path still goes through `AggRows::Refs` and must be untouched.
#[test]
fn round656_join_aggregates_are_unchanged() {
    let mut e = seeded();
    e.execute("CREATE TABLE t2 (id INT, tag TEXT)").unwrap();
    for i in 1..=50 {
        e.execute(&format!("INSERT INTO t2 VALUES ({i}, 'tag{}')", i % 3))
            .unwrap();
    }
    assert_eq!(
        one(&mut e, "SELECT count(*) FROM d JOIN t2 ON d.id = t2.id"),
        "50"
    );
    assert_eq!(
        one(&mut e, "SELECT sum(d.id) FROM d JOIN t2 ON d.id = t2.id"),
        "1275"
    );
    assert_eq!(
        one(
            &mut e,
            "SELECT t2.tag, count(*) FROM d JOIN t2 ON d.id = t2.id \
             GROUP BY t2.tag ORDER BY t2.tag"
        ),
        "tag0|16,tag1|17,tag2|17"
    );
}

/// The reservation is deliberately NOT taken when a WHERE is present: the
/// row count is only an upper bound there, and `… WHERE id = 5` over 50M
/// rows would reserve 400 MB of pointers to hold one survivor — worse than
/// the doubling chain it replaces. This pins the observable half: a highly
/// selective scan still answers, and answers cheaply enough to run here.
#[test]
fn round657_a_selective_scan_is_not_penalised() {
    let mut e = seeded();
    assert_eq!(one(&mut e, "SELECT sum(id) FROM d WHERE id = 5"), "5");
    assert_eq!(one(&mut e, "SELECT count(*) FROM d WHERE id = 5"), "1");
    assert_eq!(one(&mut e, "SELECT count(*) FROM d WHERE s = 'row7'"), "1");
}
