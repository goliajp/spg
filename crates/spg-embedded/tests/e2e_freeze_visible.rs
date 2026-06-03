//! Sanity: after freezing rows to cold, SELECT must still see
//! them. Pre-test for the v7.7.4 row-loss investigation.

use spg_embedded::{Database, QueryResult};

// SPG's cold tier is a shadow model: SELECT * full scans
// see only the hot tier; cold rows surface via PK / index
// lookups. This test pins that behaviour so the v7.7.4
// auto-compact test asserts the right contract.
#[test]
fn full_scan_sees_only_hot_pk_lookup_sees_cold() {
    let mut db = Database::open_in_memory();
    db.execute("CREATE TABLE t (id INT NOT NULL, name TEXT)").unwrap();
    db.execute("CREATE INDEX t_pk ON t (id)").unwrap();
    for i in 0..100 {
        db.execute(&format!("INSERT INTO t VALUES ({i}, 'x')")).unwrap();
    }
    db.freeze_oldest_to_cold("t", "t_pk", 50).unwrap();
    // Full scan = hot tier only.
    let full = match db.execute("SELECT id FROM t").unwrap() {
        QueryResult::Rows { rows, .. } => rows.len(),
        _ => panic!("rows"),
    };
    assert_eq!(full, 50, "SELECT * full scan returns hot-tier only");
    // PK lookup for an id that ended up in cold = surfaces.
    let cold_id = match db.execute("SELECT id FROM t WHERE id = 7").unwrap() {
        QueryResult::Rows { rows, .. } => rows.len(),
        _ => 0,
    };
    assert_eq!(cold_id, 1, "PK lookup surfaces cold row");
}
