//! v7.31 (memory campaign, design v1) — bucket meters + the byte
//! budget at the result choke point. Round-26 ask 1's instrument:
//! `Engine::memory_stats()` / `SELECT * FROM spg_memory_stats` must
//! tell an operator where resident bytes live, and any SELECT —
//! single-table included, not just the join paths — must respect
//! `max_query_bytes`.

use spg_engine::{Engine, EngineError, QueryResult};
use spg_storage::Value;

fn ok(eng: &mut Engine, sql: &str) {
    eng.execute(sql)
        .unwrap_or_else(|e| panic!("{sql:?}: {e:?}"));
}

fn seeded() -> Engine {
    let mut e = Engine::new();
    ok(&mut e, "CREATE TABLE notes (id BIGINT, body TEXT)");
    ok(&mut e, "CREATE INDEX notes_id ON notes (id)");
    ok(
        &mut e,
        "INSERT INTO notes VALUES (1, 'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa'), (2, NULL), (3, 'bbbb')",
    );
    e
}

#[test]
fn memory_stats_reports_rows_and_grows_with_data() {
    let mut e = seeded();
    let s1 = e.memory_stats();
    let t1 = s1
        .tables
        .iter()
        .find(|t| t.name == "notes")
        .expect("notes bucket present");
    assert_eq!(t1.hot_rows, 3);
    assert_eq!(t1.index_count, 1);
    // Resident accounting must at least cover the 32-byte body
    // payload plus per-cell slots.
    assert!(t1.approx_resident_bytes > 32, "{t1:?}");
    let before = t1.approx_resident_bytes;
    ok(
        &mut e,
        "INSERT INTO notes VALUES (4, 'cccccccccccccccccccccccccccccccc')",
    );
    let s2 = e.memory_stats();
    let t2 = s2.tables.iter().find(|t| t.name == "notes").unwrap();
    assert_eq!(t2.hot_rows, 4);
    assert!(
        t2.approx_resident_bytes > before,
        "resident meter must grow with data: {before} -> {}",
        t2.approx_resident_bytes
    );
    assert!(s2.total_approx_resident_bytes >= t2.approx_resident_bytes);
}

#[test]
fn memory_stats_carries_the_query_budget() {
    let e = Engine::new().with_max_query_bytes(12345);
    assert_eq!(e.memory_stats().max_query_bytes, Some(12345));
    let e2 = Engine::new();
    assert_eq!(e2.memory_stats().max_query_bytes, None);
}

#[test]
fn spg_memory_stats_virtual_table_matches_api() {
    let mut e = seeded();
    let api = e.memory_stats();
    match e.execute("SELECT * FROM spg_memory_stats").unwrap() {
        QueryResult::Rows { columns, rows } => {
            assert_eq!(columns.len(), 7);
            assert_eq!(rows.len(), api.tables.len());
            let notes = rows
                .iter()
                .find(|r| r.values[0] == Value::Text("notes".into()))
                .expect("notes row");
            assert_eq!(notes.values[1], Value::BigInt(3)); // hot_rows
            let api_notes = api.tables.iter().find(|t| t.name == "notes").unwrap();
            assert_eq!(
                notes.values[4],
                Value::BigInt(i64::try_from(api_notes.approx_resident_bytes).unwrap())
            );
        }
        other => panic!("expected Rows, got {other:?}"),
    }
}

#[test]
fn single_table_select_respects_byte_budget() {
    // No join involved — the result choke point must enforce the
    // budget for plain scans too.
    let mut e = Engine::new().with_max_query_bytes(64);
    ok(&mut e, "CREATE TABLE fat (id BIGINT, body TEXT)");
    ok(
        &mut e,
        "INSERT INTO fat VALUES (1, 'xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx')",
    );
    let err = e.execute("SELECT * FROM fat").unwrap_err();
    assert!(
        matches!(err, EngineError::QueryBytesExceeded(64)),
        "{err:?}"
    );
    // Sub-budget projections still work.
    match e.execute("SELECT id FROM fat").unwrap() {
        QueryResult::Rows { rows, .. } => assert_eq!(rows.len(), 1),
        other => panic!("expected Rows, got {other:?}"),
    }
    // Writes never route through the result budget.
    ok(&mut e, "INSERT INTO fat VALUES (2, 'yyyy')");
}
