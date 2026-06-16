//! v7.34 (crash-recovery P0 #2) — engine-level row-level redo: real SQL
//! execution captures a physical redo log that replays to identical
//! committed state. This is the capture≡execute differential at the SQL
//! level (the storage-layer per-op differentials live in spg-storage unit
//! tests); together they pin that row-level WAL recovery reproduces the
//! engine's state WITHOUT re-running the SQL — the fix for the superlinear
//! statement-replay hang root-caused on the mailrs crash-recovery P0.

use spg_engine::Engine;

fn with_ddl() -> Engine {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT PRIMARY KEY, v TEXT)")
        .unwrap();
    e.execute("CREATE TABLE u (k TEXT, n INT)").unwrap();
    e
}

#[test]
fn engine_redo_capture_replays_to_identical_state() {
    // E1 executes the DML with capture on; E2 starts from the same DDL
    // and only ever sees the captured redo — never the SQL.
    let mut e1 = with_ddl();
    e1.set_redo_capture(true);

    let mut log = Vec::new();
    for sql in [
        "INSERT INTO t VALUES (1, 'a')",
        "INSERT INTO t VALUES (2, 'b')",
        "INSERT INTO t VALUES (3, 'c')",
        "INSERT INTO u VALUES ('x', 10), ('y', 20)", // multi-row
        "UPDATE t SET v = 'B' WHERE id = 2",
        "DELETE FROM t WHERE id = 1",
        "SELECT * FROM t",                   // read → captures nothing
        "UPDATE t SET v = v WHERE id = 999", // matches nothing → no redo
    ] {
        e1.execute(sql).unwrap();
        log.extend(e1.take_redo());
    }

    let mut e2 = with_ddl();
    e2.apply_redo(&log).unwrap();

    assert_eq!(
        e1.snapshot(),
        e2.snapshot(),
        "engine redo replay diverged from execution"
    );
}

#[test]
fn redo_capture_off_by_default() {
    // Without enabling capture, take_redo is always empty (zero overhead
    // path for in-memory use).
    let mut e = with_ddl();
    e.execute("INSERT INTO t VALUES (1, 'a')").unwrap();
    assert!(e.take_redo().is_empty());
}

#[test]
fn failed_statement_captures_no_redo() {
    // A statement that errors must leave no redo (and must not leak into
    // the next statement's capture).
    let mut e = with_ddl();
    e.set_redo_capture(true);
    e.execute("INSERT INTO t VALUES (1, 'a')").unwrap();
    let _ = e.take_redo();
    // PK violation → Err.
    assert!(e.execute("INSERT INTO t VALUES (1, 'dup')").is_err());
    assert!(e.take_redo().is_empty(), "failed statement left redo");
}
