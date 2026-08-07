//! v7.39 (round 212) — EXCLUDE constraints Phase 2: multi-column. The
//! canonical booking shape `EXCLUDE USING gist (room WITH =, during WITH &&)`
//! = no two rows overlap WITHIN the same room. Live-PG18.4 differential
//! (2026-07-18, with btree_gist, which PG needs for the scalar `=` element):
//!   (1,[1,5)) OK; (2,[1,5)) OK (different room); (1,[3,7)) →
//!   ERROR:  conflicting key value violates exclusion constraint "book_room_during_excl"
//!   DETAIL: Key (room, during)=(1, [3,7)) conflicts with existing key (room, during)=(1, [1,5)).
//!   pg_constraint: conname=book_room_during_excl, contype='x', conkey='{1,2}'
//!   pg_get_constraintdef → 'EXCLUDE USING gist (room WITH =, during WITH &&)'
//! SPG needs no btree_gist extension — its O(n) enforcement evaluates `=`
//! natively, so it accepts the shape PG gates behind an extension (a superset).
//! Semantics: two DISTINCT rows conflict iff EVERY element's operator holds
//! (room1 = room2 AND during1 && during2).

use spg_engine::{Engine, QueryResult};

fn rows(e: &mut Engine, sql: &str) -> Vec<Vec<String>> {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => rows
            .into_iter()
            .map(|r| {
                r.values
                    .iter()
                    .map(|v| match v {
                        spg_storage::Value::Null => "NULL".to_string(),
                        spg_storage::Value::Text(s) => s.to_string(),
                        other => format!("{other:?}"),
                    })
                    .collect()
            })
            .collect(),
        other => panic!("{other:?}"),
    }
}

fn booking() -> Engine {
    let mut e = Engine::new();
    e.execute(
        "CREATE TABLE book (room int, during int4range, \
         EXCLUDE USING gist (room WITH =, during WITH &&))",
    )
    .unwrap();
    e
}

#[test]
fn same_room_overlap_rejected_other_room_ok() {
    let mut e = booking();
    e.execute("INSERT INTO book VALUES (1, '[1,5)')").unwrap();
    // Different room → no conflict even though the ranges overlap.
    e.execute("INSERT INTO book VALUES (2, '[1,5)')").unwrap();
    // Same room + overlapping range → conflict.
    let err = e
        .execute("INSERT INTO book VALUES (1, '[3,7)')")
        .unwrap_err()
        .to_string();
    assert!(
        err.contains(
            "conflicting key value violates exclusion constraint \"book_room_during_excl\""
        ),
        "main: {err}"
    );
    assert!(
        err.contains(
            "Key (room, during)=(1, [3,7)) conflicts with existing key (room, during)=(1, [1,5))."
        ),
        "detail: {err}"
    );
}

#[test]
fn same_room_non_overlap_ok() {
    let mut e = booking();
    e.execute("INSERT INTO book VALUES (1, '[1,5)')").unwrap();
    // Same room, adjacent (5 exclusive) → no overlap.
    e.execute("INSERT INTO book VALUES (1, '[5,10)')").unwrap();
}

#[test]
fn null_element_exempts_row() {
    // A NULL in any constrained column exempts the row (all-element AND
    // can never be true when one operand is NULL).
    let mut e = booking();
    e.execute("INSERT INTO book VALUES (1, '[1,5)')").unwrap();
    e.execute("INSERT INTO book VALUES (NULL, '[1,5)')")
        .unwrap();
    e.execute("INSERT INTO book VALUES (1, NULL)").unwrap();
}

#[test]
fn pg_constraint_and_constraintdef_multicol() {
    let mut e = booking();
    assert_eq!(
        rows(
            &mut e,
            "SELECT conname, contype, conkey FROM pg_constraint WHERE contype = 'x'"
        ),
        vec![vec![
            "book_room_during_excl".to_string(),
            "x".to_string(),
            "{1,2}".to_string(),
        ]]
    );
    assert_eq!(
        rows(
            &mut e,
            "SELECT pg_get_constraintdef(oid) FROM pg_constraint \
             WHERE conname = 'book_room_during_excl'"
        ),
        vec![vec![
            "EXCLUDE USING gist (room WITH =, during WITH &&)".to_string()
        ]]
    );
}

#[test]
fn update_into_same_room_overlap_rejected() {
    let mut e = booking();
    e.execute("INSERT INTO book VALUES (1, '[1,5)')").unwrap();
    e.execute("INSERT INTO book VALUES (2, '[1,5)')").unwrap();
    // Move room-2's booking into room 1 where it now overlaps.
    let err = e
        .execute("UPDATE book SET room = 1, during = '[4,6)' WHERE room = 2")
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("conflicting key value violates exclusion constraint"),
        "{err}"
    );
}

#[test]
fn single_column_autoname_unchanged() {
    // Regression: the single-column auto-name stays `<t>_<col>_excl`.
    let mut e = Engine::new();
    e.execute("CREATE TABLE ov (during int4range, EXCLUDE USING gist (during WITH &&))")
        .unwrap();
    assert_eq!(
        rows(
            &mut e,
            "SELECT conname FROM pg_constraint WHERE contype = 'x'"
        ),
        vec![vec!["ov_during_excl".to_string()]]
    );
}
