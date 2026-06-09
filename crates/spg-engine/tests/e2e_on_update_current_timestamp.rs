//! v7.17.0 Phase 2.1 — MySQL `ON UPDATE CURRENT_TIMESTAMP`
//! column attribute. Pre-v7.17 SPG silently accepted the syntax
//! and never fired the override; this asserts the new automatic-
//! refresh semantics on UPDATE.

use spg_engine::Engine;

fn wall_clock_us() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_micros() as i64
}

fn engine_with_real_clock() -> Engine {
    Engine::new().with_clock(wall_clock_us)
}

#[test]
fn on_update_refreshes_when_column_not_in_set() {
    let mut e = engine_with_real_clock();
    e.execute(
        "CREATE TABLE t (\
            id INT NOT NULL, \
            name TEXT NOT NULL, \
            updated_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP \
                ON UPDATE CURRENT_TIMESTAMP\
         )",
    )
    .unwrap();
    e.execute("INSERT INTO t (id, name) VALUES (1, 'alice')")
        .unwrap();
    let initial = read_updated_at(&mut e);
    // Sleep so the engine clock ticks past the initial INSERT.
    std::thread::sleep(std::time::Duration::from_millis(20));
    e.execute("UPDATE t SET name = 'alicia' WHERE id = 1")
        .unwrap();
    let after = read_updated_at(&mut e);
    assert!(
        after > initial,
        "updated_at should advance after UPDATE; got initial={initial}, after={after}"
    );
}

#[test]
fn on_update_skipped_when_column_explicitly_set() {
    let mut e = engine_with_real_clock();
    e.execute(
        "CREATE TABLE t (\
            id INT NOT NULL, \
            name TEXT NOT NULL, \
            updated_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP \
                ON UPDATE CURRENT_TIMESTAMP\
         )",
    )
    .unwrap();
    e.execute("INSERT INTO t (id, name) VALUES (1, 'a')")
        .unwrap();
    // Caller-supplied value wins per MySQL semantics.
    e.execute("UPDATE t SET name = 'b', updated_at = '2030-01-01 00:00:00'::TIMESTAMP")
        .unwrap();
    let v = read_updated_at(&mut e);
    // Two big numbers — should not equal "right now"-ish.
    assert!(v > 1_700_000_000_000_000, "should be a real timestamp");
    // Approximate check: 2030 epoch micros is ~ 1893456000000000.
    assert!(
        (1_893_400_000_000_000..1_893_500_000_000_000).contains(&v),
        "explicit value should win, got {v}"
    );
}

#[test]
fn on_update_with_precision_parens_accepted() {
    let mut e = engine_with_real_clock();
    // PG / MySQL precision is parser-only — engine stores TIMESTAMP
    // at microsecond resolution regardless of N.
    e.execute(
        "CREATE TABLE t (\
            id INT NOT NULL, \
            updated_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP(6) \
                ON UPDATE CURRENT_TIMESTAMP(6)\
         )",
    )
    .unwrap();
    e.execute("INSERT INTO t (id) VALUES (1)").unwrap();
    e.execute("UPDATE t SET id = 2").unwrap();
}

#[test]
fn on_update_only_fires_for_updated_rows() {
    let mut e = engine_with_real_clock();
    e.execute(
        "CREATE TABLE t (\
            id INT NOT NULL, \
            v INT NOT NULL, \
            updated_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP \
                ON UPDATE CURRENT_TIMESTAMP\
         )",
    )
    .unwrap();
    e.execute("INSERT INTO t (id, v) VALUES (1, 10), (2, 20)")
        .unwrap();
    let t1_before = read_updated_at_for(&mut e, 1);
    let t2_before = read_updated_at_for(&mut e, 2);
    std::thread::sleep(std::time::Duration::from_millis(20));
    e.execute("UPDATE t SET v = 99 WHERE id = 1").unwrap();
    let t1_after = read_updated_at_for(&mut e, 1);
    let t2_after = read_updated_at_for(&mut e, 2);
    assert!(t1_after > t1_before, "row 1 should refresh");
    assert_eq!(t2_before, t2_after, "row 2 should NOT refresh");
}

#[test]
fn round_trip_preserves_on_update_binding() {
    let mut e = engine_with_real_clock();
    e.execute(
        "CREATE TABLE t (\
            id INT NOT NULL, \
            updated_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP \
                ON UPDATE CURRENT_TIMESTAMP\
         )",
    )
    .unwrap();
    let snapshot = e.catalog().serialize();
    let restored = spg_storage::Catalog::deserialize(&snapshot).expect("round-trip");
    let table = restored.get("t").expect("table persisted");
    let col = table
        .schema()
        .columns
        .iter()
        .find(|c| c.name == "updated_at")
        .expect("column");
    assert!(col.on_update_runtime.is_some(), "binding preserved");
}

// ----- helpers -----

fn read_updated_at(e: &mut Engine) -> i64 {
    let r = e.execute("SELECT updated_at FROM t").unwrap();
    let rows = match r {
        spg_engine::QueryResult::Rows { rows, .. } => rows,
        _ => panic!("expected rows"),
    };
    match &rows[0].values[0] {
        spg_storage::Value::Timestamp(t) => *t,
        other => panic!("not a timestamp: {other:?}"),
    }
}

fn read_updated_at_for(e: &mut Engine, id: i32) -> i64 {
    let r = e
        .execute(&format!("SELECT updated_at FROM t WHERE id = {id}"))
        .unwrap();
    let rows = match r {
        spg_engine::QueryResult::Rows { rows, .. } => rows,
        _ => panic!("expected rows"),
    };
    match &rows[0].values[0] {
        spg_storage::Value::Timestamp(t) => *t,
        other => panic!("not a timestamp: {other:?}"),
    }
}
