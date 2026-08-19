//! 7.38.2 S-2 — `RETURNING (xmax = 0) AS is_new`, the PG upsert idiom.
//!
//! sentori report 5 (2026-08-18): their ingest distinguishes "row was
//! inserted" from "row was conflict-updated" with PG's
//! `INSERT … ON CONFLICT DO UPDATE … RETURNING (xmax = 0) AS is_new`.
//! PG18 anchors (differential 2026-08-19): plain INSERT → xmax 0, the
//! conflict-updated row → nonzero, UPDATE's new tuple → 0, DELETE's old
//! tuple → nonzero. SPG's in-place MVCC keeps live headers at xmax = 0,
//! so the DML paths synthesise the per-statement answer.

use spg_engine::{Engine, QueryResult};

fn row1(e: &mut Engine, sql: &str) -> Vec<String> {
    match e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err}")) {
        QueryResult::Rows { rows, .. } => rows[0]
            .values
            .iter()
            .map(spg_engine::eval::value_to_text)
            .collect(),
        other => panic!("{sql}: {other:?}"),
    }
}

#[test]
fn pin_v7382_upsert_returning_is_new() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE up (id INT PRIMARY KEY, v TEXT)")
        .unwrap();
    let first = row1(
        &mut e,
        "INSERT INTO up VALUES (1, 'a') ON CONFLICT (id) DO UPDATE \
         SET v = excluded.v RETURNING id, (xmax = 0) AS is_new",
    );
    assert_eq!(first, vec!["1", "true"], "fresh row must read is_new");
    let second = row1(
        &mut e,
        "INSERT INTO up VALUES (1, 'b') ON CONFLICT (id) DO UPDATE \
         SET v = excluded.v RETURNING id, (xmax = 0) AS is_new",
    );
    assert_eq!(
        second,
        vec!["1", "false"],
        "conflict-updated row must not read is_new"
    );
}

#[test]
fn pin_v7382_returning_xmax_shapes() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE shp (id INT PRIMARY KEY, v TEXT)")
        .unwrap();
    // Bare `RETURNING xmax` keeps PG's column name and reads 0 on INSERT.
    match e
        .execute("INSERT INTO shp VALUES (1, 'a') RETURNING xmax")
        .unwrap()
    {
        QueryResult::Rows { columns, rows } => {
            assert_eq!(columns[0].name, "xmax");
            assert_eq!(spg_engine::eval::value_to_text(&rows[0].values[0]), "0");
        }
        other => panic!("{other:?}"),
    }
    // UPDATE returns the new tuple: xmax 0.
    let upd = row1(&mut e, "UPDATE shp SET v = 'b' WHERE id = 1 RETURNING xmax");
    assert_eq!(upd, vec!["0"]);
    // DELETE returns the old tuple: xmax nonzero (the deleter).
    let del = row1(&mut e, "DELETE FROM shp WHERE id = 1 RETURNING xmax");
    assert_ne!(del, vec!["0"], "DELETE RETURNING xmax must be nonzero");
}

#[test]
fn pin_v7382_user_column_named_xmax_wins() {
    // PG reserves the name; SPG lets a user claim it, and then the user
    // column must win over the synthetic one — same rule as the scan path.
    let mut e = Engine::new();
    e.execute("CREATE TABLE claimed (id INT, xmax INT)")
        .unwrap();
    let got = row1(&mut e, "INSERT INTO claimed VALUES (1, 42) RETURNING xmax");
    assert_eq!(got, vec!["42"]);
}
