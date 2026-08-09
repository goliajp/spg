//! After freezing rows to cold, `SELECT` still sees them — all of them.
//!
//! r944 changed what this file pins, and the change is worth stating.
//! It used to assert the opposite: "SPG's cold tier is a shadow model:
//! SELECT * full scans see only the hot tier; cold rows surface via PK /
//! index lookups", written for the v7.7.4 row-loss investigation.
//!
//! v7.35.1 had already overruled that from the executor side, in a
//! comment on the scan itself: folding cold rows into the full-scan loop
//! because "pre-7.35.1 it only walked `table.rows()` (hot), so any
//! `SELECT … FROM t` against a table with cold segments silently
//! returned a subset". A full scan that omits rows is not a storage
//! model, it is a wrong answer, and PG has nothing like it.
//!
//! The old assertion kept passing anyway, because the fold only worked
//! when the scan happened to look in the same index the freeze wrote to
//! (round 943). With round 944 making the scan look wherever the freeze
//! filed the locators, the full scan returns all 100 rows and the old
//! expectation of 50 fails. The expectation is what was wrong.

use spg_embedded::{Database, QueryResult};

#[test]
fn a_full_scan_sees_frozen_rows_and_so_does_a_pk_lookup() {
    let mut db = Database::open_in_memory();
    db.execute("CREATE TABLE t (id INT NOT NULL, name TEXT)")
        .unwrap();
    db.execute("CREATE INDEX t_pk ON t (id)").unwrap();
    for i in 0..100 {
        db.execute(&format!("INSERT INTO t VALUES ({i}, 'x')"))
            .unwrap();
    }
    db.freeze_oldest_to_cold("t", "t_pk", 50).unwrap();
    // Full scan = every row, hot and cold.
    let full = match db.execute("SELECT id FROM t").unwrap() {
        QueryResult::Rows { rows, .. } => rows.len(),
        _ => panic!("rows"),
    };
    assert_eq!(
        full, 100,
        "a full scan must return the frozen rows too, not the hot half"
    );
    // PK lookup for an id that ended up in cold = surfaces.
    let cold_id = match db.execute("SELECT id FROM t WHERE id = 7").unwrap() {
        QueryResult::Rows { rows, .. } => rows.len(),
        _ => 0,
    };
    assert_eq!(cold_id, 1, "PK lookup surfaces cold row");
}
