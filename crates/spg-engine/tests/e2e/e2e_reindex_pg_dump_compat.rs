//! v7.37.17 (17.6 sibling) — REINDEX statement pg_dump-compat.

use spg_engine::Engine;

fn ddl(e: &mut Engine, sql: &str) {
    e.execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"));
}

#[test]
fn reindex_index_no_op() {
    let mut e = Engine::new();
    ddl(&mut e, "CREATE TABLE t (id INT)");
    ddl(&mut e, "CREATE INDEX idx_t_id ON t (id)");
    ddl(&mut e, "REINDEX INDEX idx_t_id");
}

#[test]
fn reindex_table_no_op() {
    let mut e = Engine::new();
    ddl(&mut e, "CREATE TABLE t (id INT)");
    ddl(&mut e, "REINDEX TABLE t");
}

#[test]
fn reindex_concurrently_index_no_op() {
    let mut e = Engine::new();
    ddl(&mut e, "CREATE TABLE t (id INT)");
    ddl(&mut e, "CREATE INDEX idx_t_id ON t (id)");
    ddl(&mut e, "REINDEX CONCURRENTLY INDEX idx_t_id");
}

#[test]
fn reindex_schema_no_op() {
    let mut e = Engine::new();
    ddl(&mut e, "REINDEX SCHEMA public");
}

#[test]
fn reindex_database_no_op() {
    let mut e = Engine::new();
    ddl(&mut e, "REINDEX DATABASE mydb");
}

#[test]
fn reindex_with_verbose_option_no_op() {
    let mut e = Engine::new();
    ddl(&mut e, "CREATE TABLE t (id INT)");
    ddl(&mut e, "CREATE INDEX idx_t_id ON t (id)");
    ddl(&mut e, "REINDEX (VERBOSE) INDEX idx_t_id");
}
