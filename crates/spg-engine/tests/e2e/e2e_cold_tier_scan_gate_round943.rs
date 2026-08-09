//! r943 — cold rows are written under one index and looked for under
//! another, and they disappear. IGNORED: these reproduce an OPEN bug.
//!
//! **These cases fail on HEAD with no change applied.** A scan over a
//! table with 15 of 40 rows frozen returns 25. Verified against a clean
//! tree and a forced rebuild, because the first reading came while an
//! experiment was in the working copy and that is exactly when a
//! pre-existing failure gets blamed on the experiment.
//!
//! The write side: `freezer.rs:pick_target` chooses the first BTree
//! index over ANY integer column, and `freeze_oldest_to_cold` writes the
//! cold locators into that one index.
//!
//! The read side: `iter_cold_rows_of_table` requires a single-column
//! PRIMARY KEY, then takes the first BTree index whose column position
//! is the PK's. That is not necessarily the index the freeze wrote to.
//! When the two disagree the walk finds no `Cold` locators, and the rows
//! are simply absent — no error, no warning, a short answer.
//!
//! The setup below is the smallest shape that shows it: a PK on `id` and
//! a separate `by_id` index on the same column, frozen through `by_id`.
//! The existing freeze tests do not catch it because they either use a
//! table with no PRIMARY KEY (so the reader declines and returns nothing
//! either way) or read through `AS OF SEGMENT`, which resolves segments
//! directly rather than through this walk.
//!
//! This is the 7.35.1 bug's shape returning by another route: back then
//! the full-scan executor walked only the hot tier and silently returned
//! a subset. Version 7.35.1 taught it to fold in cold rows; it can still
//! fold in none of them.
//!
//! `join.rs` gates four paths on `has_cold_rows_fast()`, which is a
//! second exposure of the same kind: `freeze_oldest_to_cold` never calls
//! `set_cold_row_count` or `mark_cold_row_count_stale`, so that
//! predicate answers "no cold rows" while cold rows exist.
//!
//! Un-ignore these when the read side is taught to find the locators
//! wherever the freeze put them.

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
#[ignore = "reproduces an OPEN bug: cold rows written under one index are looked for under another"]
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
#[ignore = "reproduces an OPEN bug: cold rows written under one index are looked for under another"]
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
#[ignore = "reproduces an OPEN bug: cold rows written under one index are looked for under another"]
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
#[ignore = "reproduces an OPEN bug: cold rows written under one index are looked for under another"]
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
