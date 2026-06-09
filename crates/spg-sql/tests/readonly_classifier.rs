//! v7.18 — `Statement::is_readonly()` classifier tests.
//!
//! Backs the spg-sqlx Pool full-support track: SpgConnection
//! routes readonly statements through AsyncReadHandle (snapshot,
//! fan-out) and writer statements through the writer mutex.
//! Anything misclassified here breaks the contract either by
//! routing a write through an immutable snapshot (which would
//! error at execute time, but should never reach that path) or
//! by serialising a SELECT through the writer mutex.

use spg_sql::parser::parse_statement;

fn ro(sql: &str) -> bool {
    parse_statement(sql)
        .unwrap_or_else(|e| panic!("parse failed for {sql:?}: {e:?}"))
        .is_readonly()
}

#[test]
fn select_is_readonly() {
    assert!(ro("SELECT 1"));
    assert!(ro("SELECT a, b FROM t WHERE c = $1"));
    assert!(ro(
        "SELECT t1.a, t2.b FROM t1 JOIN t2 ON t1.id = t2.id LIMIT 10"
    ));
    assert!(ro(
        "WITH cte AS (SELECT * FROM t) SELECT * FROM cte WHERE id > $1"
    ));
}

#[test]
fn explain_is_readonly() {
    assert!(ro("EXPLAIN SELECT 1"));
    assert!(ro("EXPLAIN ANALYZE SELECT * FROM t"));
}

#[test]
fn show_family_is_readonly() {
    assert!(ro("SHOW TABLES"));
    assert!(ro("SHOW DATABASES"));
    assert!(ro("SHOW CREATE TABLE t"));
    assert!(ro("SHOW INDEXES FROM t"));
    assert!(ro("SHOW STATUS"));
    assert!(ro("SHOW VARIABLES"));
    assert!(ro("SHOW PROCESSLIST"));
    assert!(ro("SHOW COLUMNS FROM t"));
}

#[test]
fn dml_is_writer() {
    assert!(!ro("INSERT INTO t VALUES (1)"));
    assert!(!ro("UPDATE t SET a = 1 WHERE id = $1"));
    assert!(!ro("DELETE FROM t WHERE id = $1"));
}

#[test]
fn ddl_is_writer() {
    assert!(!ro("CREATE TABLE t (a INT)"));
    assert!(!ro("DROP TABLE t"));
    assert!(!ro("CREATE INDEX idx ON t (a)"));
    assert!(!ro("DROP INDEX idx"));
    assert!(!ro("ALTER TABLE t ADD COLUMN b INT"));
}

#[test]
fn tx_control_is_writer() {
    assert!(!ro("BEGIN"));
    assert!(!ro("COMMIT"));
    assert!(!ro("ROLLBACK"));
    assert!(!ro("SAVEPOINT sp"));
    assert!(!ro("ROLLBACK TO SAVEPOINT sp"));
    assert!(!ro("RELEASE SAVEPOINT sp"));
}

#[test]
fn session_state_is_writer() {
    // SET / RESET mutate engine session state — must run on the
    // writer that owns the session.
    assert!(!ro("SET search_path = public"));
    assert!(!ro("RESET search_path"));
    assert!(!ro("RESET ALL"));
}

#[test]
fn analyze_is_writer() {
    // ANALYZE mutates statistics; even though it looks "read-
    // shaped" it's a writer-path command.
    assert!(!ro("ANALYZE"));
    assert!(!ro("ANALYZE t"));
}

#[test]
fn empty_is_writer_by_default() {
    // No-op routing through the writer is a defensive default —
    // any future side effect from Empty lands uniformly.
    assert!(!ro(""));
    assert!(!ro("-- just a comment"));
}
