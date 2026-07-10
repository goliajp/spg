//! v7.37.17 (17.6 siblings) — pg_dump-compat maintenance statements:
//! VACUUM / CLUSTER / LISTEN / NOTIFY / UNLISTEN / LOCK / CHECKPOINT.

use spg_engine::Engine;

fn ddl(e: &mut Engine, sql: &str) {
    e.execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"));
}

#[test]
fn vacuum_bare_no_op() {
    let mut e = Engine::new();
    ddl(&mut e, "VACUUM");
}

#[test]
fn vacuum_table_no_op() {
    let mut e = Engine::new();
    ddl(&mut e, "CREATE TABLE t (id INT)");
    ddl(&mut e, "VACUUM t");
}

#[test]
fn vacuum_analyze_full_no_op() {
    let mut e = Engine::new();
    ddl(&mut e, "CREATE TABLE t (id INT)");
    ddl(&mut e, "VACUUM (FULL, ANALYZE, VERBOSE) t");
}

#[test]
fn cluster_no_op() {
    let mut e = Engine::new();
    ddl(&mut e, "CREATE TABLE t (id INT)");
    ddl(&mut e, "CREATE INDEX idx_t_id ON t (id)");
    ddl(&mut e, "CLUSTER t USING idx_t_id");
}

#[test]
fn listen_notify_unlisten_no_op() {
    let mut e = Engine::new();
    ddl(&mut e, "LISTEN chan_a");
    ddl(&mut e, "NOTIFY chan_a, 'payload'");
    ddl(&mut e, "UNLISTEN chan_a");
    ddl(&mut e, "UNLISTEN *");
}

#[test]
fn lock_table_no_op() {
    let mut e = Engine::new();
    ddl(&mut e, "CREATE TABLE t (id INT)");
    ddl(&mut e, "LOCK TABLE t");
    ddl(&mut e, "LOCK TABLE t IN EXCLUSIVE MODE");
    ddl(&mut e, "LOCK TABLE t IN ACCESS SHARE MODE NOWAIT");
}

// CHECKPOINT at the *engine* layer is a no-op: the no_std engine
// owns no WAL / snapshot, so there is nothing to flush here. The
// real durability barrier is wired at the HOST layer (embedded
// `Database::execute_buffered` → `Database::checkpoint`, Epic Du);
// see spg-embedded's `bare_checkpoint_forces_real_checkpoint` test.
#[test]
fn checkpoint_engine_layer_no_op() {
    let mut e = Engine::new();
    ddl(&mut e, "CHECKPOINT");
}
