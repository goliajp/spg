//! v7.39 (round 211) — EXCLUDE constraints Phase 1: catalog reflection so a
//! pg_dump round-trips. Live-PG18.4 differential (2026-07-18):
//!   CREATE TABLE ov (room int, during int4range,
//!                    EXCLUDE USING gist (during WITH &&));
//!   pg_constraint: conname=ov_during_excl, contype='x', conkey='{2}'
//!   pg_get_constraintdef(oid) → 'EXCLUDE USING gist (during WITH &&)'
//! PG's information_schema.table_constraints does NOT list EXCLUDE (the
//! SQL-standard view covers only CHECK / FK / PK / UNIQUE), so SPG omits
//! it there too — matching PG means NOT adding it.

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

fn setup() -> Engine {
    let mut e = Engine::new();
    e.execute(
        "CREATE TABLE ov (room int, during int4range, \
         EXCLUDE USING gist (during WITH &&))",
    )
    .unwrap();
    e
}

#[test]
fn pg_constraint_reports_exclude() {
    let mut e = setup();
    assert_eq!(
        rows(
            &mut e,
            "SELECT conname, contype, conkey FROM pg_constraint \
             WHERE contype = 'x'"
        ),
        vec![vec![
            "ov_during_excl".to_string(),
            "x".to_string(),
            "{2}".to_string(),
        ]]
    );
}

#[test]
fn pg_get_constraintdef_deparses_exclude() {
    let mut e = setup();
    assert_eq!(
        rows(
            &mut e,
            "SELECT pg_get_constraintdef(oid) FROM pg_constraint \
             WHERE conname = 'ov_during_excl'"
        ),
        vec![vec!["EXCLUDE USING gist (during WITH &&)".to_string()]]
    );
}

#[test]
fn information_schema_omits_exclude_like_pg() {
    // PG's SQL-standard table_constraints view never lists EXCLUDE.
    let mut e = setup();
    assert_eq!(
        rows(
            &mut e,
            "SELECT constraint_name FROM information_schema.table_constraints \
             WHERE table_name = 'ov' AND constraint_type = 'EXCLUDE'"
        ),
        Vec::<Vec<String>>::new()
    );
}

#[test]
fn alter_add_exclude_reflected() {
    // The ALTER TABLE ADD path (what pg_dump emits) also lands in the catalog.
    let mut e = Engine::new();
    e.execute("CREATE TABLE bk (during int4range)").unwrap();
    e.execute("ALTER TABLE bk ADD CONSTRAINT bk_no_overlap EXCLUDE USING gist (during WITH &&)")
        .unwrap();
    assert_eq!(
        rows(
            &mut e,
            "SELECT pg_get_constraintdef(oid) FROM pg_constraint \
             WHERE conname = 'bk_no_overlap'"
        ),
        vec![vec!["EXCLUDE USING gist (during WITH &&)".to_string()]]
    );
    // …and it enforces after the ALTER.
    e.execute("INSERT INTO bk VALUES ('[1,5)')").unwrap();
    assert!(e.execute("INSERT INTO bk VALUES ('[3,7)')").is_err());
}
