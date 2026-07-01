//! v7.37.17 (17.6 partial) — CREATE INDEX CONCURRENTLY noise word.

use spg_engine::Engine;

fn ddl(e: &mut Engine, sql: &str) {
    e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err:?}"));
}

#[test]
fn create_index_concurrently_parses_and_works() {
    let mut e = Engine::new();
    ddl(&mut e, "CREATE TABLE t (id INT, name TEXT)");
    ddl(&mut e, "INSERT INTO t VALUES (1, 'alice')");
    ddl(&mut e, "CREATE INDEX CONCURRENTLY idx_t_id ON t (id)");
    // Confirm the index actually built and is usable.
    let r = e.execute("SELECT id FROM t WHERE id = 1").unwrap();
    let spg_engine::QueryResult::Rows { rows, .. } = r else {
        panic!("Rows");
    };
    assert_eq!(rows.len(), 1);
}

#[test]
fn create_unique_index_concurrently_parses() {
    let mut e = Engine::new();
    ddl(&mut e, "CREATE TABLE t (id INT)");
    ddl(&mut e, "CREATE UNIQUE INDEX CONCURRENTLY idx_t_id ON t (id)");
}

#[test]
fn create_index_concurrently_if_not_exists_parses() {
    let mut e = Engine::new();
    ddl(&mut e, "CREATE TABLE t (id INT)");
    ddl(
        &mut e,
        "CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_t_id ON t (id)",
    );
    ddl(
        &mut e,
        "CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_t_id ON t (id)",
    );
}
