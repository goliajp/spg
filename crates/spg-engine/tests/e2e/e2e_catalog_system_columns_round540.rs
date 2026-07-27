//! v7.39 (round 540) — a catalog relation carries the system columns.
//!
//! Round 539's `pg_dump` run stopped on its extension query, and the
//! diagnosis was exact: a system column resolved on a bare scan and not
//! through a join.
//!
//!     SELECT x.tableoid FROM pg_extension x                    answers
//!     SELECT x.tableoid FROM pg_extension x JOIN … ON …        does not
//!
//! Round 512 materialised the six for a user TABLE on the bare-select
//! scan, which is not where a catalog is read from: a synthesized view
//! becomes a real relation when it is materialised, and the join path
//! builds its schema from that relation's columns. Appending them once,
//! at the point every catalog passes through, is what makes them
//! resolve wherever the relation is read.
//!
//! `*` still skips the trailing six by POSITION — round 512's rule, so
//! a genuine `xmin` column is not lost — and that skip now sees them
//! through a join's `alias.column` naming too.
//!
//! Every expectation below is a PG18 reading.

use spg_engine::{Engine, QueryResult};

fn engine() -> Engine {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (a INT)").unwrap();
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

fn columns(e: &mut Engine, sql: &str) -> Vec<String> {
    match e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err}")) {
        QueryResult::Rows { columns, .. } => columns.iter().map(|c| c.name.clone()).collect(),
        other => panic!("{sql}: {other:?}"),
    }
}

/// The shape pg_dump asks: a system column through a join.
#[test]
fn round540_system_column_resolves_through_a_join() {
    let mut e = engine();
    let bare = rows(&mut e, "SELECT c.tableoid FROM pg_class c LIMIT 1");
    let joined = rows(
        &mut e,
        "SELECT c.tableoid FROM pg_class c JOIN pg_namespace n \
         ON n.oid = c.relnamespace LIMIT 1",
    );
    assert_eq!(bare, joined, "a join must not change what tableoid answers");
    // And it is PG's own oid for that catalog, not the internal name's.
    assert_eq!(bare, vec!["1259"]);
}

/// Every one of the six answers on a catalog.
#[test]
fn round540_all_six_answer_on_a_catalog() {
    let mut e = engine();
    assert_eq!(
        rows(
            &mut e,
            "SELECT ctid, xmin, cmin, xmax, cmax, tableoid FROM pg_class LIMIT 1"
        ),
        // One block, offsets from 1; a catalog row is frozen, which is
        // what PG reports for one too.
        vec!["(0,1)|2|0|0|0|1259"]
    );
}

/// `*` does not show them — bare, joined, or on information_schema.
#[test]
fn round540_star_still_skips_them() {
    let mut e = engine();
    for sql in [
        "SELECT * FROM pg_class",
        "SELECT * FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace",
        "SELECT * FROM information_schema.tables",
    ] {
        let cols = columns(&mut e, sql);
        for sys in ["ctid", "xmin", "cmin", "xmax", "cmax", "tableoid"] {
            assert!(
                !cols.iter().any(|c| c.rsplit('.').next() == Some(sys)),
                "{sql} leaked {sys}"
            );
        }
    }
}

/// A catalog with a column genuinely called `xmin` keeps it — the
/// reason round 512 made the skip positional.
#[test]
fn round540_a_real_xmin_column_survives() {
    let mut e = engine();
    let cols = columns(&mut e, "SELECT * FROM pg_replication_slots");
    assert!(cols.iter().any(|c| c == "xmin"), "columns were {cols:?}");
}

/// A user table is unaffected: still bare-scan only, and still skipped
/// from `*`.
#[test]
fn round540_user_table_unchanged() {
    let mut e = engine();
    e.execute("INSERT INTO t VALUES (1)").unwrap();
    assert_eq!(rows(&mut e, "SELECT ctid FROM t"), vec!["(0,1)"]);
    assert_eq!(columns(&mut e, "SELECT * FROM t"), vec!["a"]);
}
