//! v7.17.0 Phase 8 / N6 audit remainder — rarely-emitted
//! pg_dump shapes that should load as Empty no-op.

use spg_engine::Engine;

#[test]
fn create_statistics_parses_as_noop() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (a INT NOT NULL, b INT NOT NULL)")
        .unwrap();
    // PG 12+ emits this in dumps with extended statistics objects.
    e.execute("CREATE STATISTICS stat_ab ON a, b FROM t").unwrap();
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
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT NOT NULL)").unwrap();
    e.execute("CREATE STATISTICS stat ON id FROM t").unwrap();
    e.execute("INSERT INTO t VALUES (1), (2)").unwrap();
    let r = e.execute("SELECT count(*) FROM t").unwrap();
    let spg_engine::QueryResult::Rows { rows, .. } = r else {
        panic!()
    };
    assert!(!rows.is_empty());
}
