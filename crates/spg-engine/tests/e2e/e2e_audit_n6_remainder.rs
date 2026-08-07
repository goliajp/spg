//! v7.17.0 Phase 8 / N6 audit remainder — rarely-emitted
//! pg_dump shapes that should load as Empty no-op.

use spg_engine::Engine;

#[test]
fn create_statistics_parses_as_noop() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (a INT NOT NULL, b INT NOT NULL)")
        .unwrap();
    // PG 12+ emits this in dumps with extended statistics objects.
    e.execute("CREATE STATISTICS stat_ab ON a, b FROM t")
        .unwrap();
}

#[test]
fn create_event_trigger_parses_as_noop() {
    let mut e = Engine::new();
    e.execute(
        "CREATE EVENT TRIGGER trg_audit \
         ON ddl_command_end \
         EXECUTE PROCEDURE audit_ddl()",
    )
    .unwrap();
}

#[test]
fn create_foreign_table_parses_as_noop() {
    let mut e = Engine::new();
    // PG FDW emits CREATE FOREIGN TABLE in dumps when fdws are
    // configured.
    e.execute(
        "CREATE FOREIGN TABLE remote_users (id INT, name TEXT) \
         SERVER my_server OPTIONS (table_name 'users')",
    )
    .unwrap();
}

#[test]
fn create_statistics_does_not_block_following_dml() {
    // v7.39 (round 280) — CREATE STATISTICS is a real catalog object
    // now, so this needs a legal one. `ON id` alone is rejected BY PG
    // ("extended statistics require at least 2 columns"); the pin used
    // it only because the statement was being swallowed.
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT NOT NULL, tag TEXT)")
        .unwrap();
    e.execute("CREATE STATISTICS stat ON id, tag FROM t")
        .unwrap();
    e.execute("INSERT INTO t VALUES (1, 'a'), (2, 'b')")
        .unwrap();
    let r = e.execute("SELECT count(*) FROM t").unwrap();
    let spg_engine::QueryResult::Rows { rows, .. } = r else {
        panic!()
    };
    assert!(!rows.is_empty());
}
