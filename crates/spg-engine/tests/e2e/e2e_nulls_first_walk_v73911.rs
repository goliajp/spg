//! v7.39.11 — `ORDER BY <indexed nullable column> NULLS FIRST` with a
//! LIMIT dropped every NULL row.
//!
//! This is the r1020 defect, still reachable through the one spelling
//! its guard did not cover. r1020 refused the index walk when the key
//! was nullable and the order was DESC, because DESC defaults to NULLS
//! FIRST and a walk cannot produce rows that are not in the tree. An
//! explicit `NULLS FIRST` on an ASCENDING order puts the NULLs in the
//! same place and was not refused.
//!
//! Measured on PostgreSQL 18.6, `(1,2,3,4,5,NULL,6)`:
//!
//! ```text
//!   ORDER BY a NULLS FIRST LIMIT 3          PG: NULL 1 2
//!   every published SPG through 7.39.10:        1 2 3
//! ```
//!
//! Without the index both answer `NULL 1 2` — the sort sees every row —
//! which is why it takes an index to see this at all, and why the
//! fixtures below build one. The control is the same query with no
//! index: the two must agree.

use spg_engine::{Engine, QueryResult};

fn col(e: &mut Engine, sql: &str) -> Vec<String> {
    let QueryResult::Rows { rows, .. } = e.execute(sql).unwrap_or_else(|x| panic!("{sql}: {x:?}"))
    else {
        panic!("{sql}: expected Rows");
    };
    rows.iter()
        .map(|r| spg_engine::eval::value_to_text(&r.values[0]))
        .collect()
}

fn seeded(with_index: bool) -> Engine {
    let mut e = Engine::new();
    e.execute("CREATE TABLE nf (a int, b int)").unwrap();
    e.execute("INSERT INTO nf VALUES (1,10),(2,20),(3,30),(4,40),(5,NULL),(NULL,60),(6,70)")
        .unwrap();
    if with_index {
        e.execute("CREATE INDEX nf_a ON nf (a)").unwrap();
    }
    e
}

/// Every ordering below, asked of both fixtures. An index may make a
/// query faster; it may never make it answer differently.
fn same_with_and_without_the_index(sql: &str, expected: &[&str]) {
    let with = col(&mut seeded(true), sql);
    let without = col(&mut seeded(false), sql);
    assert_eq!(without, expected, "{sql}: without the index");
    assert_eq!(with, expected, "{sql}: WITH the index");
}

#[test]
fn ascending_nulls_first_keeps_the_null_rows() {
    same_with_and_without_the_index(
        "SELECT a FROM nf ORDER BY a NULLS FIRST LIMIT 3",
        &["NULL", "1", "2"],
    );
}

#[test]
fn ascending_nulls_first_with_an_offset_too() {
    same_with_and_without_the_index(
        "SELECT a FROM nf ORDER BY a NULLS FIRST OFFSET 1 LIMIT 2",
        &["1", "2"],
    );
}

#[test]
fn descending_nulls_last_is_the_other_half_of_the_same_correction() {
    // r1020's guard refused this one for no reason: its NULLs belong at
    // the end, which is the side a walk can serve.
    same_with_and_without_the_index(
        "SELECT a FROM nf ORDER BY a DESC NULLS LAST LIMIT 3",
        &["6", "5", "4"],
    );
}

#[test]
fn the_defaults_are_unchanged() {
    // Ascending defaults to NULLS LAST, descending to NULLS FIRST.
    same_with_and_without_the_index("SELECT a FROM nf ORDER BY a LIMIT 3", &["1", "2", "3"]);
    same_with_and_without_the_index(
        "SELECT a FROM nf ORDER BY a DESC LIMIT 3",
        &["NULL", "6", "5"],
    );
}

#[test]
fn an_unbounded_order_was_never_wrong_and_still_is_not() {
    // The sort sees every row, so this one always agreed with
    // PostgreSQL; it is here so a future change to the walk cannot
    // move it without being noticed.
    same_with_and_without_the_index(
        "SELECT a FROM nf ORDER BY a NULLS FIRST",
        &["NULL", "1", "2", "3", "4", "5", "6"],
    );
}
