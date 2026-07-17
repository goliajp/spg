//! v7.9.29/30 — CREATE UNIQUE INDEX [WHERE pred] enforcement.
//! mailrs migration K1: business uniqueness invariants
//! (email_templates default, calendar_events master/instance).

use spg_engine::{Engine, EngineError, QueryResult};

fn exec(eng: &mut Engine, sql: &str) -> Result<QueryResult, EngineError> {
    eng.execute(sql)
}

fn ok(eng: &mut Engine, sql: &str) {
    exec(eng, sql).unwrap_or_else(|e| panic!("{sql:?}: {e:?}"));
}

fn err_contains(eng: &mut Engine, sql: &str, needle: &str) {
    match exec(eng, sql) {
        Ok(r) => panic!("expected error for {sql:?}, got {r:?}"),
        Err(e) => {
            let msg = format!("{e:?}");
            assert!(
                msg.contains(needle),
                "expected error to contain {needle:?}, got {msg}"
            );
        }
    }
}

#[test]
fn unique_index_basic_blocks_dup() {
    let mut eng = Engine::new();
    ok(&mut eng, "CREATE TABLE t (a INT NOT NULL, b INT NOT NULL)");
    ok(&mut eng, "CREATE UNIQUE INDEX uq_a ON t (a)");
    ok(&mut eng, "INSERT INTO t VALUES (1, 100)");
    ok(&mut eng, "INSERT INTO t VALUES (2, 200)");
    err_contains(
        &mut eng,
        "INSERT INTO t VALUES (1, 999)",
        "unique constraint",
    );
}

#[test]
fn unique_index_allows_null() {
    // PG semantics: UNIQUE allows multiple NULLs.
    let mut eng = Engine::new();
    ok(&mut eng, "CREATE TABLE t (a INT, b INT NOT NULL)");
    ok(&mut eng, "CREATE UNIQUE INDEX uq_a ON t (a)");
    ok(&mut eng, "INSERT INTO t VALUES (NULL, 1)");
    ok(&mut eng, "INSERT INTO t VALUES (NULL, 2)");
    ok(&mut eng, "INSERT INTO t VALUES (NULL, 3)");
}

#[test]
fn partial_unique_index_filters_by_predicate() {
    // mailrs email_templates pattern: "one default per user".
    let mut eng = Engine::new();
    ok(
        &mut eng,
        "CREATE TABLE email_templates (\
            user_address TEXT NOT NULL, \
            name TEXT NOT NULL, \
            is_default BOOL NOT NULL\
        )",
    );
    ok(
        &mut eng,
        "CREATE UNIQUE INDEX idx_email_templates_user_default \
         ON email_templates (user_address) WHERE is_default = true",
    );
    // Many non-default templates per user — allowed.
    ok(
        &mut eng,
        "INSERT INTO email_templates VALUES ('a@x', 't1', false)",
    );
    ok(
        &mut eng,
        "INSERT INTO email_templates VALUES ('a@x', 't2', false)",
    );
    // One default per user — allowed.
    ok(
        &mut eng,
        "INSERT INTO email_templates VALUES ('a@x', 'd1', true)",
    );
    // Another user's default — allowed.
    ok(
        &mut eng,
        "INSERT INTO email_templates VALUES ('b@x', 'd1', true)",
    );
    // Second default for the same user — must reject.
    err_contains(
        &mut eng,
        "INSERT INTO email_templates VALUES ('a@x', 'd2', true)",
        "unique constraint",
    );
}

#[test]
fn partial_unique_caldav_master_instance() {
    // mailrs CalDAV pattern: two partial-unique indexes over the
    // same table, distinguishing master events (recurrence_id IS NULL)
    // from instance overrides (recurrence_id IS NOT NULL).
    let mut eng = Engine::new();
    ok(
        &mut eng,
        "CREATE TABLE calendar_events (\
            calendar_id INT NOT NULL, \
            uid TEXT NOT NULL, \
            recurrence_id TEXT\
        )",
    );
    ok(
        &mut eng,
        "CREATE UNIQUE INDEX uq_cal_master ON calendar_events \
         (calendar_id, uid) WHERE recurrence_id IS NULL",
    );
    ok(
        &mut eng,
        "CREATE UNIQUE INDEX uq_cal_instance ON calendar_events \
         (calendar_id, uid, recurrence_id) WHERE recurrence_id IS NOT NULL",
    );
    // Same calendar — one master per uid + many distinct instances.
    ok(
        &mut eng,
        "INSERT INTO calendar_events VALUES (1, 'uid-a', NULL)",
    );
    ok(
        &mut eng,
        "INSERT INTO calendar_events VALUES (1, 'uid-a', '2026-01-01')",
    );
    ok(
        &mut eng,
        "INSERT INTO calendar_events VALUES (1, 'uid-a', '2026-02-01')",
    );
    // Different uid in same calendar — also allowed.
    ok(
        &mut eng,
        "INSERT INTO calendar_events VALUES (1, 'uid-b', NULL)",
    );
    // Duplicate master (same calendar_id + uid + NULL recurrence) — reject.
    err_contains(
        &mut eng,
        "INSERT INTO calendar_events VALUES (1, 'uid-a', NULL)",
        "unique constraint",
    );
    // Duplicate instance (same triple) — reject.
    err_contains(
        &mut eng,
        "INSERT INTO calendar_events VALUES (1, 'uid-a', '2026-01-01')",
        "unique constraint",
    );
}

#[test]
fn partial_unique_within_batch_dup() {
    let mut eng = Engine::new();
    ok(
        &mut eng,
        "CREATE TABLE t (a INT NOT NULL, hot BOOL NOT NULL)",
    );
    ok(
        &mut eng,
        "CREATE UNIQUE INDEX uq_a_hot ON t (a) WHERE hot = true",
    );
    // Two rows with a=1 but hot=false → both allowed.
    ok(&mut eng, "INSERT INTO t VALUES (1, false), (1, false)");
    // Same batch, a=2 twice with hot=true → reject.
    err_contains(
        &mut eng,
        "INSERT INTO t VALUES (2, true), (2, true)",
        "unique constraint",
    );
}

#[test]
fn create_unique_index_rejects_pre_existing_dup() {
    let mut eng = Engine::new();
    ok(&mut eng, "CREATE TABLE t (a INT NOT NULL)");
    ok(&mut eng, "INSERT INTO t VALUES (1)");
    ok(&mut eng, "INSERT INTO t VALUES (1)");
    // v7.39 (read01 round 52) — PG's wording (23505 at the wire). The helper
    // matches against the Debug form, which backslash-escapes the quotes
    // around the index name, so assert on the unquoted core.
    err_contains(
        &mut eng,
        "CREATE UNIQUE INDEX uq_a ON t (a)",
        "could not create unique index",
    );
    // …and the failed index must not be left behind (PG's CREATE is atomic).
    ok(&mut eng, "CREATE INDEX uq_a ON t (a)");
}

#[test]
fn unique_index_persists_across_snapshot() {
    let mut eng = Engine::new();
    ok(&mut eng, "CREATE TABLE t (a INT NOT NULL)");
    ok(&mut eng, "CREATE UNIQUE INDEX uq_a ON t (a)");
    ok(&mut eng, "INSERT INTO t VALUES (1), (2), (3)");
    let bytes = eng.snapshot();
    // Re-open from the snapshot envelope — the is_unique flag must survive.
    let mut eng2 = Engine::restore_envelope(&bytes).expect("reload");
    err_contains(&mut eng2, "INSERT INTO t VALUES (2)", "unique constraint");
    ok(&mut eng2, "INSERT INTO t VALUES (4)");
}
