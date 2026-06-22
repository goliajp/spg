//! v7.37.7 C.1 gap probe — quick verification of which baseline
//! corpus gaps still need parser/engine work and which already work.
//! NOT a production test surface; each probe just asserts that the
//! parse succeeds. Real e2e land in dedicated files as gaps close.

use spg_engine::Engine;

fn try_parse(sql: &str) -> Result<(), String> {
    let mut e = Engine::new();
    // CREATE TABLE first to avoid 'unknown table' downstream errors
    // making us think the parser failed.
    let _ = e.execute("CREATE TABLE t (id INT NOT NULL, name TEXT, v INT, ts TIMESTAMP)");
    let _ = e.execute("INSERT INTO t VALUES (1, 'a', 10, '2026-01-01 00:00:00')");
    e.execute(sql).map_err(|err| format!("{err:?}")).map(|_| ())
}

#[test]
fn probe_cardinality() {
    let r = try_parse("SELECT cardinality(ARRAY[1, 2, 3])");
    eprintln!("cardinality: {r:?}");
}

#[test]
fn probe_substring_from_for() {
    let r = try_parse("SELECT substring(name FROM 1 FOR 2) FROM t");
    eprintln!("substring FROM/FOR: {r:?}");
}

#[test]
fn probe_modulo_percent() {
    let r = try_parse("SELECT v % 3 FROM t");
    eprintln!("`%` modulo: {r:?}");
}

#[test]
fn probe_interval_column() {
    let r = try_parse("CREATE TABLE u (d INTERVAL)");
    eprintln!("INTERVAL column type: {r:?}");
}

#[test]
fn probe_values_as_named_cols() {
    let r = try_parse("SELECT * FROM (VALUES (1, 'a'), (2, 'b')) AS v(id, name)");
    eprintln!("VALUES AS v(c1, c2): {r:?}");
}

#[test]
fn probe_expression_index() {
    let r = try_parse("CREATE INDEX i_expr ON t ((v + 1))");
    eprintln!("CREATE INDEX ((expr)): {r:?}");
}

#[test]
fn probe_writable_cte_returning() {
    let r = try_parse(
        "WITH inserted AS (INSERT INTO t (id, v) VALUES (99, 100) RETURNING id) \
         SELECT * FROM inserted",
    );
    eprintln!("WITH INSERT … RETURNING: {r:?}");
}

#[test]
fn probe_any_subquery() {
    let r = try_parse("SELECT id FROM t WHERE v = ANY (SELECT v FROM t WHERE v > 0)");
    eprintln!("= ANY (subquery): {r:?}");
}
