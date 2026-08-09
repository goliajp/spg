//! r943/r944 — cold rows are written under one index and looked for
//! under another. Found failing in r943, fixed in r944.
//!
//! The symptom: a table with 15 of 40 rows frozen answered a plain
//! `SELECT` with 25 rows. No error, no warning — a short answer.
//!
//! The two sides had chosen their index independently.
//! `freezer.rs:pick_target` took the first BTree index over ANY integer
//! column and the freeze filed the locators there;
//! `iter_cold_rows_of_table` required a single-column PRIMARY KEY and
//! then took the first BTree index on that column. When those are not
//! the same index the walk finds no `Cold` locators at all.
//!
//! r944 gives both sides one rule — `Table::btree_index_key_is_unique` —
//! and makes the reader union over every index that satisfies it. Unique
//! is the requirement because `resolve_cold_locator` resolves BY KEY: a
//! non-unique index cannot say which of two rows sharing a key was
//! meant, so freezing through one files rows that cannot be recovered.
//!
//! This is the 7.35.1 bug's shape returning by another route: back then
//! the full-scan executor walked only the hot tier and silently returned
//! a subset. 7.35.1 taught it to fold in cold rows; it could still fold
//! in none of them.
//!
//! The existing freeze tests miss all of this: they either build a table
//! with no PRIMARY KEY, so the old reader declined and returned nothing
//! either way, or they read through `AS OF SEGMENT`, which resolves
//! segments directly instead of through this walk.

use spg_engine::{Engine, QueryResult};

fn rows_of(e: &mut Engine, sql: &str) -> Vec<String> {
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

/// 40 rows in `t`, a matching lookup table, and the oldest 15 frozen.
fn seeded_and_frozen() -> Engine {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT PRIMARY KEY, k INT, pad TEXT)")
        .unwrap();
    e.execute("CREATE INDEX by_id ON t (id)").unwrap();
    e.execute("CREATE TABLE peer (id INT PRIMARY KEY, label TEXT)")
        .unwrap();
    for id in 1..=40i32 {
        e.execute(&format!(
            "INSERT INTO t VALUES ({id}, {}, 'p{id}')",
            (id * 7) % 40
        ))
        .unwrap();
        e.execute(&format!("INSERT INTO peer VALUES ({id}, 'L{id}')"))
            .unwrap();
    }
    e.freeze_oldest_to_cold("t", "by_id", 15).unwrap();
    e
}

/// The simplest form: a plain scan should see all 40 after the freeze.
/// It returns 25. No join, no gate, no predicate — the read side just
/// looks in the wrong index.
#[test]
fn round943_a_plain_scan_sees_frozen_rows() {
    let mut e = seeded_and_frozen();
    assert_eq!(
        rows_of(&mut e, "SELECT id FROM t ORDER BY id").len(),
        40,
        "the full scan lost frozen rows"
    );
}

/// A join over the same table. `join.rs` gates several of its paths on
/// `has_cold_rows_fast()`, and that predicate is false here while 15
/// rows are cold.
#[test]
fn round943_a_join_sees_frozen_rows() {
    let mut e = seeded_and_frozen();
    let joined = rows_of(
        &mut e,
        "SELECT t.id FROM t JOIN peer ON peer.id = t.id ORDER BY t.id",
    );
    assert_eq!(
        joined.len(),
        40,
        "a join dropped frozen rows: got {} of 40",
        joined.len()
    );
    let want: Vec<String> = (1..=40).map(|i| i.to_string()).collect();
    assert_eq!(joined, want);
}

/// The same join with a predicate that only frozen rows satisfy, so a
/// dropped cold tier cannot hide behind the hot rows.
#[test]
fn round943_a_join_filtered_to_the_frozen_half() {
    let mut e = seeded_and_frozen();
    let joined = rows_of(
        &mut e,
        "SELECT t.id, peer.label FROM t JOIN peer ON peer.id = t.id WHERE t.id <= 15 ORDER BY t.id",
    );
    assert_eq!(joined.len(), 15, "the frozen half vanished from the join");
    assert_eq!(joined[0], "1|L1");
}

/// An aggregate over the join, which is where a missing row shows up as
/// a wrong number rather than a short list.
#[test]
fn round943_an_aggregate_over_the_join_counts_frozen_rows() {
    let mut e = seeded_and_frozen();
    assert_eq!(
        rows_of(&mut e, "SELECT count(*) FROM t JOIN peer ON peer.id = t.id"),
        vec!["40".to_string()]
    );
    assert_eq!(
        rows_of(
            &mut e,
            "SELECT sum(t.id) FROM t JOIN peer ON peer.id = t.id"
        ),
        vec!["820".to_string()],
        "1..40 sums to 820; a short sum means rows went missing"
    );
}

/// The write side's half of the rule, stated as the property that must
/// hold however the layers below choose to answer: freezing through a
/// non-unique index must never cost a row.
///
/// `resolve_cold_locator` resolves BY KEY, so under an index where four
/// rows share a key it cannot say which was meant. r944 makes the
/// freezer decline such an index; this asserts the consequence, which is
/// what a caller can actually observe, rather than the private choice.
#[test]
fn round944_freezing_through_a_non_unique_index_costs_no_rows() {
    let mut e = Engine::new();
    // `k` is indexed and integer, but four rows share each value.
    e.execute("CREATE TABLE dup (id INT PRIMARY KEY, k INT)")
        .unwrap();
    e.execute("CREATE INDEX by_k ON dup (k)").unwrap();
    for id in 1..=20i32 {
        e.execute(&format!("INSERT INTO dup VALUES ({id}, {})", id % 5))
            .unwrap();
    }
    let before = rows_of(&mut e, "SELECT id FROM dup ORDER BY id");
    assert_eq!(before.len(), 20, "seed");

    // Accepted or refused, the answer afterwards is the same 20 rows.
    let _ = e.freeze_oldest_to_cold("dup", "by_k", 8);

    let after = rows_of(&mut e, "SELECT id FROM dup ORDER BY id");
    assert_eq!(
        after, before,
        "freezing through a non-unique index changed the answer"
    );
    assert_eq!(
        rows_of(&mut e, "SELECT count(*) FROM dup"),
        vec!["20".to_string()]
    );
}
