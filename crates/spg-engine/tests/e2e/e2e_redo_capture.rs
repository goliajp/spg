//! v7.34 (crash-recovery P0 #2) — engine-level row-level redo: real SQL
//! execution captures a physical redo log that replays to identical
//! committed state. This is the capture≡execute differential at the SQL
//! level (the storage-layer per-op differentials live in spg-storage unit
//! tests); together they pin that row-level WAL recovery reproduces the
//! engine's state WITHOUT re-running the SQL — the fix for the superlinear
//! statement-replay hang root-caused on the mailrs crash-recovery P0.

use spg_engine::{Engine, RowChange};

/// The writer version stamped on a captured change (Epic W slice 2).
fn writer_version(c: &RowChange) -> u64 {
    match c {
        RowChange::Insert { writer_version, .. }
        | RowChange::Update { writer_version, .. }
        | RowChange::Delete { writer_version, .. } => *writer_version,
        // A tombstone's `xmax` IS its writer version (the deleting
        // statement's version). Only the gate-on (`SPG_MVCC_INPLACE`)
        // path emits it; these e2e tests run gate-off, so it never fires.
        RowChange::Tombstone { xmax, .. } => *xmax,
    }
}

fn with_ddl() -> Engine {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT PRIMARY KEY, v TEXT)")
        .unwrap();
    e.execute("CREATE TABLE u (k TEXT, n INT)").unwrap();
    e
}

/// v7.37.16 (Epic W) — the v53 catalog snapshot persists each row's stable
/// `RowId`, and those ids are allocated path-dependently: redo replay's
/// `set_rows_and_rebuild_indices` mints FRESH ids rather than reproducing
/// the exact ids a direct SQL execution would hand out. So `snapshot()`
/// bytes legitimately diverge between execution and replay on the id
/// bookkeeping alone (the rows + their visibility are identical). Assert
/// LOGICAL committed-state equivalence — the rows a client actually
/// observes — via SELECT, which is id-independent and the property these
/// capture≡execute tests care about.
fn assert_same_rows(e1: &mut Engine, e2: &mut Engine, query: &str) {
    let r1 = e1.execute(query).unwrap();
    let r2 = e2.execute(query).unwrap();
    assert_eq!(r1, r2, "redo replay diverged from execution on `{query}`");
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

    // Logical committed state must match (id-independent; see
    // `assert_same_rows`).
    assert_same_rows(&mut e1, &mut e2, "SELECT * FROM t ORDER BY id");
    assert_same_rows(&mut e1, &mut e2, "SELECT * FROM u ORDER BY k, n");
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

// v7.37.15 (Epic W slice 2) — the redo now carries the REAL committing
// writer version, no longer the hard-coded `0` of slice 1.
//
// XMIN_FROZEN = 1 is the reserved frozen version; every real committing
// version is strictly greater, so `> 1` proves a genuine minted value.
const XMIN_FROZEN: u64 = spg_engine::XMIN_FROZEN;

#[test]
fn redo_carries_real_autocommit_writer_version() {
    let mut e = with_ddl();
    e.set_redo_capture(true);

    // Each autocommit statement mints exactly one writer version and
    // stamps it on every change it produces. The global version counter
    // is monotonic, so versions across sequential statements are strictly
    // increasing — this holds regardless of any concurrent test minting.
    e.execute("INSERT INTO u VALUES ('a', 1), ('b', 2), ('c', 3)")
        .unwrap();
    let ins = e.take_redo();
    assert_eq!(ins.len(), 3, "3 rows → 3 Insert changes");
    let v_ins = writer_version(&ins[0]);
    assert!(
        v_ins > XMIN_FROZEN,
        "insert writer_version must be a real minted version (> XMIN_FROZEN), got {v_ins}"
    );
    // All rows of one statement share the one committing version.
    for c in &ins {
        assert_eq!(
            writer_version(c),
            v_ins,
            "all changes from one statement share its writer version"
        );
    }

    e.execute("UPDATE u SET n = 99 WHERE k = 'a'").unwrap();
    let upd = e.take_redo();
    // Under the mvcc-inplace-on verification feature an UPDATE is
    // tombstone(old) + insert(new) — two changes, one shared version.
    let expect_upd = if !cfg!(feature = "mvcc-inplace-off") { 2 } else { 1 };
    assert_eq!(upd.len(), expect_upd);
    let v_upd = writer_version(&upd[0]);
    for c in &upd {
        assert_eq!(
            writer_version(c),
            v_upd,
            "all changes from one UPDATE share its writer version"
        );
    }
    assert!(v_upd > v_ins, "later statement → strictly greater version");

    e.execute("DELETE FROM u WHERE k = 'b'").unwrap();
    let del = e.take_redo();
    assert_eq!(del.len(), 1);
    let v_del = writer_version(&del[0]);
    assert!(
        v_del > v_upd,
        "delete's committing version strictly greater than the update's"
    );
}

#[test]
fn redo_in_explicit_tx_shares_one_committing_version() {
    // Inside an explicit transaction, `writer_version_for_current_stmt`
    // returns the tx's single pre-allocated version for EVERY statement
    // (deterministic — no per-statement allocation), so all the redo the
    // tx produces carries one identical, non-zero committing version.
    // This is the deterministic proof that the drain stamps exactly the
    // version the engine reports for the current statement.
    let mut e = with_ddl();
    e.set_redo_capture(true);

    e.execute("BEGIN").unwrap();
    let mut tx_changes = Vec::new();
    e.execute("INSERT INTO u VALUES ('x', 10), ('y', 20)")
        .unwrap();
    tx_changes.extend(e.take_redo());
    e.execute("UPDATE u SET n = 11 WHERE k = 'x'").unwrap();
    tx_changes.extend(e.take_redo());
    e.execute("DELETE FROM u WHERE k = 'y'").unwrap();
    tx_changes.extend(e.take_redo());
    e.execute("COMMIT").unwrap();

    assert!(
        tx_changes.len() >= 3,
        "insert(2) + update + delete produced redo, got {}",
        tx_changes.len()
    );
    let v0 = writer_version(&tx_changes[0]);
    assert!(v0 > XMIN_FROZEN, "tx writer version must be real, got {v0}");
    for c in &tx_changes {
        assert_eq!(
            writer_version(c),
            v0,
            "every change in one tx shares the tx's single committing version"
        );
    }
}

#[test]
fn writer_version_stamp_does_not_change_replay_result() {
    // Slice-2 constraint: `writer_version` is carried-but-unused by
    // replay. Capturing WITH the real version must replay to exactly the
    // same committed state as executing the SQL directly — identical to
    // slice 1, where the field was 0. Replay resolves by physical
    // position and never reads `writer_version`.
    let mut e1 = with_ddl();
    e1.set_redo_capture(true);
    let mut log = Vec::new();
    for sql in [
        "INSERT INTO t VALUES (1, 'a')",
        "INSERT INTO t VALUES (2, 'b')",
        "UPDATE t SET v = 'B' WHERE id = 2",
        "DELETE FROM t WHERE id = 1",
    ] {
        e1.execute(sql).unwrap();
        log.extend(e1.take_redo());
    }
    // Every captured change carries a real (non-zero) version …
    assert!(log.iter().all(|c| writer_version(c) > XMIN_FROZEN));

    // … yet replay reproduces the executed committed state (id-independent;
    // see `assert_same_rows`).
    let mut e2 = with_ddl();
    e2.apply_redo(&log).unwrap();
    assert_same_rows(&mut e1, &mut e2, "SELECT * FROM t ORDER BY id");
}
