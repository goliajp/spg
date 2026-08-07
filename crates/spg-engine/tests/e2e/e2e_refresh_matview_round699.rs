//! Round 699 — `REFRESH MATERIALIZED VIEW` gave one sentence for two
//! different failures.
//!
//! Measured against PG18:
//!
//!   name is missing         PG `relation "x" does not exist`
//!   name exists, wrong kind PG `"x" is not a materialized view`
//!   SPG, for both           `materialized view "x" does not exist`
//!
//! The second is the one that costs a caller time. SPG's sentence says the
//! name was not found; PG's says the name WAS found and the object is not
//! what the statement is for. Those send you to different places.
//!
//! Both rode `StorageError::Corrupt`, the wrapper round 698 caught putting
//! `corrupt on-disk format: ` in front of an ordinary typo. They ride
//! `Unsupported` now, which carries no banner, and the missing-name wording
//! is the one the wire's classifier already reads as 42P01.
//!
//! The rest of that batch of ten already agreed with PG18: DISCARD,
//! DEALLOCATE, CLOSE, FETCH, MOVE and EXECUTE over a missing name, SET
//! CONSTRAINTS over a missing constraint, LISTEN/NOTIFY/UNLISTEN, and
//! CREATE INDEX CONCURRENTLY over a missing relation.

use spg_engine::Engine;

fn err_of(e: &mut Engine, sql: &str) -> String {
    format!(
        "{}",
        e.execute(sql).expect_err(&format!("PG18 refuses: {sql}"))
    )
}

#[test]
fn round699_a_missing_name_reads_as_a_missing_relation() {
    let mut e = Engine::new();
    let err = err_of(&mut e, "REFRESH MATERIALIZED VIEW nosuch699");
    assert!(
        err.contains("relation \"nosuch699\" does not exist"),
        "{err}"
    );
    // The banner round 698 was about must not be back.
    assert!(!err.contains("corrupt on-disk format"), "{err}");
}

#[test]
fn round699_a_name_that_is_not_a_matview_says_so() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE plain699(i INT)").unwrap();
    let err = err_of(&mut e, "REFRESH MATERIALIZED VIEW plain699");
    assert!(
        err.contains("\"plain699\" is not a materialized view"),
        "{err}"
    );
    // And specifically NOT the missing-name wording, which is what made the
    // two indistinguishable before.
    assert!(!err.contains("does not exist"), "{err}");
}

/// A real materialized view still refreshes — the split must not have cost
/// the working path.
#[test]
fn round699_a_real_matview_still_refreshes() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE src699(i INT)").unwrap();
    e.execute("INSERT INTO src699 VALUES (1),(2)").unwrap();
    e.execute("CREATE MATERIALIZED VIEW mv699 AS SELECT i FROM src699")
        .unwrap();
    e.execute("INSERT INTO src699 VALUES (3)").unwrap();
    e.execute("REFRESH MATERIALIZED VIEW mv699").unwrap();
    let n = match e.execute("SELECT count(*) FROM mv699").unwrap() {
        spg_engine::QueryResult::Rows { rows, .. } => {
            spg_engine::eval::value_to_text(&rows[0].values[0])
        }
        other => panic!("{other:?}"),
    };
    assert_eq!(n, "3");
}
