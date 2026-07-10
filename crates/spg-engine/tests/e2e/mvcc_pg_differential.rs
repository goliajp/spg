//! Phase C.3 — gate-on in-place MVCC (`SPG_MVCC_INPLACE`) differential
//! vs PostgreSQL semantics.
//!
//! ## What this proves
//! The in-place MVCC gate turns DELETE/UPDATE from a *physical* row
//! removal into a *two-state* tombstone (stamp `xmax`, keep the row
//! physically present; UPDATE additionally appends a new version).
//! Every user-visible read path is then gated on the visibility oracle
//! so the observable result is identical to a physical delete. Before
//! this gate can flip to production-default it must be proven that the
//! gate-ON visible result matches PostgreSQL on the visibility
//! behaviours the gate changes, and that gate-OFF (the control) stays
//! byte-identical to gate-ON's visible result.
//!
//! ## Harness form — live-PG18-anchored engine-level differential
//! Every expected value below was **captured from live PostgreSQL
//! 18.4** (`PostgreSQL 18.4 on aarch64-unknown-linux-gnu`, docker
//! `spg-bench-postgres`, db/role `bench`, port 25432) by running the
//! identical SQL scripts on 2026-07-04. The captured PG output is
//! encoded as the `expect` slices — this is stronger than encoding PG
//! semantics from memory/docs (it is PG's *observed* behaviour) while
//! staying hermetic (no runtime PG dependency; runs green in CI).
//!
//! Each scenario runs through:
//!   * a **gate-ON** `Engine` (`set_mvcc_inplace(true)`)  — the subject
//!   * a **gate-OFF** `Engine` (default)                  — the control
//! and asserts BOTH equal the value real PG produced. A divergence
//! makes the test fail on PG's value (never masked) — that is the point
//! of a differential.
//!
//! ## Scope boundary (honest)
//! The gate is currently a **single-session** mechanism: the server
//! holds one `RwLock<Engine>` with a single `current_tx`, and in-tx
//! gate-on writes live in a per-tx shadow catalog (cross-session
//! concurrency + concurrent checkpoint are Phase C.5, not yet built).
//! Therefore the two multi-session isolation behaviours — REPEATABLE
//! READ frozen-snapshot vs a *concurrent committer*, and READ COMMITTED
//! seeing *another session's* commit — are architecturally not
//! exercisable today and are NOT asserted here (asserting them would
//! require a second concurrent transaction the architecture cannot
//! open). What IS exercisable and asserted: the single-session RR
//! frozen-snapshot path still shows the tx's own post-snapshot writes
//! (S6), and per-statement self-visibility inside a tx (S4).

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

/// Normalise a cell to a comparable string. Integer widths (int2/int4/
/// int8 — e.g. `count(*)`/`sum()` are int8 in PG) collapse to their
/// decimal text so a PG `int8` and an SPG `Int`/`BigInt` compare equal
/// on value, matching how a client observes them.
fn cell(v: &Value) -> String {
    match v {
        Value::SmallInt(n) => n.to_string(),
        Value::Int(n) => n.to_string(),
        Value::BigInt(n) => n.to_string(),
        Value::Text(s) => s.to_string(),
        other => format!("{other:?}"),
    }
}

/// Run `sql` and return its rows as strings. Panics (with the SQL) if
/// the statement did not yield a row set.
fn sel(e: &mut Engine, sql: &str) -> Vec<Vec<String>> {
    match e
        .execute(sql)
        .unwrap_or_else(|err| panic!("exec `{sql}` failed: {err:?}"))
    {
        QueryResult::Rows { rows, .. } => rows
            .iter()
            .map(|r| r.values.iter().map(cell).collect())
            .collect(),
        _ => panic!("expected a row set from `{sql}`"),
    }
}

/// Run a non-query statement, asserting success.
fn run(e: &mut Engine, sql: &str) {
    e.execute(sql)
        .unwrap_or_else(|err| panic!("exec `{sql}` failed: {err:?}"));
}

/// A gate-configured fresh engine.
fn engine(gate_on: bool) -> Engine {
    let mut e = Engine::new();
    e.set_mvcc_inplace(gate_on);
    e
}

/// Assert a captured-from-PG expected row set. `expect` is `&[&[&str]]`.
fn assert_rows(actual: &[Vec<String>], expect: &[&[&str]], label: &str) {
    let exp: Vec<Vec<String>> = expect
        .iter()
        .map(|row| row.iter().map(|c| c.to_string()).collect())
        .collect();
    assert_eq!(
        actual, &exp,
        "MVCC differential divergence at `{label}` \
         (left = SPG, right = live PostgreSQL 18.4)"
    );
}

/// Run one scenario body against a fresh engine under BOTH gate-ON
/// (the subject) and gate-OFF (the control), so the body's PG-captured
/// asserts must hold identically in both states. gate-OFF is the
/// control: MVCC must not change observable single-session results.
fn each_gate<F: Fn(&mut Engine)>(body: F) {
    for gate_on in [true, false] {
        let mut e = engine(gate_on);
        body(&mut e);
    }
}

// ---------------------------------------------------------------------
// S1 — self-visibility in autocommit: INSERT / DELETE / UPDATE
// PG18.4 captured:
//   after INSERT (1,10),(2,20),(3,30)         => rows 1|10 2|20 3|30
//   after DELETE id=2                         => rows 1|10 3|30
//   after UPDATE v=99 WHERE id=3              => rows 1|10 3|99
// ---------------------------------------------------------------------
#[test]
fn mvcc_pg_diff_s1_autocommit_self_visibility() {
    each_gate(|e| {
        run(e, "CREATE TABLE t (id INT PRIMARY KEY, v INT)");
        run(e, "INSERT INTO t VALUES (1,10),(2,20),(3,30)");
        assert_rows(
            &sel(e, "SELECT id, v FROM t ORDER BY id"),
            &[&["1", "10"], &["2", "20"], &["3", "30"]],
            "S1 INSERT then SELECT sees own rows",
        );
        run(e, "DELETE FROM t WHERE id = 2");
        assert_rows(
            &sel(e, "SELECT id, v FROM t ORDER BY id"),
            &[&["1", "10"], &["3", "30"]],
            "S1 DELETE then SELECT hides row",
        );
        run(e, "UPDATE t SET v = 99 WHERE id = 3");
        assert_rows(
            &sel(e, "SELECT id, v FROM t ORDER BY id"),
            &[&["1", "10"], &["3", "99"]],
            "S1 UPDATE then SELECT shows new not old",
        );
    });
}

// ---------------------------------------------------------------------
// S2 — tombstone hiding under aggregation.
// PG18.4 captured: DELETE id=2 from (1,2,3) => count(*)=2, sum(id)=4.
// ---------------------------------------------------------------------
#[test]
fn mvcc_pg_diff_s2_tombstone_hidden_from_aggregate() {
    each_gate(|e| {
        run(e, "CREATE TABLE t2 (id INT)");
        run(e, "INSERT INTO t2 VALUES (1),(2),(3)");
        run(e, "DELETE FROM t2 WHERE id = 2");
        assert_rows(
            &sel(e, "SELECT count(*) AS cnt, sum(id) AS s FROM t2"),
            &[&["2", "4"]],
            "S2 aggregate excludes tombstoned row",
        );
    });
}

// ---------------------------------------------------------------------
// S3 — delete-then-reinsert of the same PK: exactly one visible row.
// PG18.4 captured: INSERT 2; DELETE 2; INSERT 2 => SELECT id=2 => one 2.
// ---------------------------------------------------------------------
#[test]
fn mvcc_pg_diff_s3_delete_then_reinsert_one_visible() {
    each_gate(|e| {
        run(e, "CREATE TABLE t3 (id INT PRIMARY KEY)");
        run(e, "INSERT INTO t3 VALUES (2)");
        run(e, "DELETE FROM t3 WHERE id = 2");
        run(e, "INSERT INTO t3 VALUES (2)");
        assert_rows(
            &sel(e, "SELECT id FROM t3 WHERE id = 2"),
            &[&["2"]],
            "S3 reinsert of a tombstoned key yields exactly one live row",
        );
    });
}

// ---------------------------------------------------------------------
// S4 — self-visibility INSIDE a transaction, incl. insert-then-delete
// of the same row in one tx.
// PG18.4 captured (base (1,10),(2,20)):
//   BEGIN; INSERT (3,30); SELECT     => 1|10 2|20 3|30
//   DELETE id=1; SELECT              => 2|20 3|30
//   UPDATE id=2 v=222; SELECT        => 2|222 3|30
//   INSERT (9,90); DELETE id=9; SEL  => 2|222 3|30   (id=9 never visible)
//   COMMIT; SELECT                   => 2|222 3|30
// ---------------------------------------------------------------------
#[test]
fn mvcc_pg_diff_s4_in_tx_self_visibility() {
    each_gate(|e| {
        run(e, "CREATE TABLE b (id INT PRIMARY KEY, v INT)");
        run(e, "INSERT INTO b VALUES (1,10),(2,20)");
        run(e, "BEGIN");
        run(e, "INSERT INTO b VALUES (3,30)");
        assert_rows(
            &sel(e, "SELECT id, v FROM b ORDER BY id"),
            &[&["1", "10"], &["2", "20"], &["3", "30"]],
            "S4 tx sees its own INSERT",
        );
        run(e, "DELETE FROM b WHERE id = 1");
        assert_rows(
            &sel(e, "SELECT id, v FROM b ORDER BY id"),
            &[&["2", "20"], &["3", "30"]],
            "S4 tx sees its own DELETE",
        );
        run(e, "UPDATE b SET v = 222 WHERE id = 2");
        assert_rows(
            &sel(e, "SELECT id, v FROM b ORDER BY id"),
            &[&["2", "222"], &["3", "30"]],
            "S4 tx sees its own UPDATE (new value)",
        );
        run(e, "INSERT INTO b VALUES (9,90)");
        run(e, "DELETE FROM b WHERE id = 9");
        assert_rows(
            &sel(e, "SELECT id, v FROM b ORDER BY id"),
            &[&["2", "222"], &["3", "30"]],
            "S4 insert-then-delete of same row in one tx: not visible",
        );
        run(e, "COMMIT");
        assert_rows(
            &sel(e, "SELECT id, v FROM b ORDER BY id"),
            &[&["2", "222"], &["3", "30"]],
            "S4 post-commit visible state",
        );
    });
}

// ---------------------------------------------------------------------
// S5 — aggregate + join + index-eq lookup over tombstoned rows.
// PG18.4 captured:
//   c: (1,a),(2,b),(3,x); o: (10,1,100),(11,1,50),(12,2,70),
//      (13,3,999),(14,2,30); DELETE c id=3; DELETE o id=13.
//   SELECT id,name FROM c WHERE id=3      => (empty)   [index-eq on tomb]
//   c JOIN o ... GROUP BY name ORDER BY   => a|2|150  b|2|100
// ---------------------------------------------------------------------
#[test]
fn mvcc_pg_diff_s5_join_aggregate_index_over_tombstones() {
    each_gate(|e| {
        run(e, "CREATE TABLE c (id INT PRIMARY KEY, name TEXT)");
        run(e, "CREATE TABLE o (id INT PRIMARY KEY, cid INT, amt INT)");
        run(e, "INSERT INTO c VALUES (1,'a'),(2,'b'),(3,'x')");
        run(
            e,
            "INSERT INTO o VALUES (10,1,100),(11,1,50),(12,2,70),(13,3,999),(14,2,30)",
        );
        run(e, "DELETE FROM c WHERE id = 3");
        run(e, "DELETE FROM o WHERE id = 13");
        // index-eq lookup on a tombstoned PK returns nothing.
        assert_rows(
            &sel(e, "SELECT id, name FROM c WHERE id = 3"),
            &[],
            "S5 index-eq lookup skips tombstoned key",
        );
        // join + aggregate excludes tombstoned parent (id=3) and the
        // tombstoned order (id=13).
        assert_rows(
            &sel(
                e,
                "SELECT c.name, count(*) AS n, sum(o.amt) AS s \
                 FROM c JOIN o ON o.cid = c.id GROUP BY c.name ORDER BY c.name",
            ),
            &[&["a", "2", "150"], &["b", "2", "100"]],
            "S5 join+aggregate excludes tombstones both sides",
        );
    });
}

// ---------------------------------------------------------------------
// S6 — REPEATABLE READ single-session: the tx's OWN post-snapshot write
// is still visible through the frozen RR snapshot (exercises the
// cached-snapshot self-write path). Cross-session RR (frozen vs a
// concurrent committer) is Phase C.5 and NOT asserted — see module doc.
// PG18.4 captured:
//   r=(1,2); BEGIN ISOLATION LEVEL REPEATABLE READ;
//   count(*)=2; INSERT 3; count(*)=3 (own write visible); COMMIT;
//   count(*)=3.
// Note: SPG's parser parse-and-ignores `ISOLATION LEVEL` on BEGIN, so
// the RR level is set via `SET TRANSACTION ISOLATION LEVEL REPEATABLE
// READ` (which `exec_begin` reads to cache the frozen snapshot).
// ---------------------------------------------------------------------
#[test]
fn mvcc_pg_diff_s6_repeatable_read_single_session_own_write() {
    each_gate(|e| {
        run(e, "CREATE TABLE r (id INT)");
        run(e, "INSERT INTO r VALUES (1),(2)");
        run(e, "SET TRANSACTION ISOLATION LEVEL REPEATABLE READ");
        run(e, "BEGIN");
        assert_rows(
            &sel(e, "SELECT count(*) FROM r"),
            &[&["2"]],
            "S6 RR snapshot sees the pre-tx committed rows",
        );
        run(e, "INSERT INTO r VALUES (3)");
        assert_rows(
            &sel(e, "SELECT count(*) FROM r"),
            &[&["3"]],
            "S6 RR frozen snapshot still shows the tx's OWN post-snapshot INSERT",
        );
        run(e, "COMMIT");
        assert_rows(
            &sel(e, "SELECT count(*) FROM r"),
            &[&["3"]],
            "S6 post-commit visible count",
        );
    });
}
