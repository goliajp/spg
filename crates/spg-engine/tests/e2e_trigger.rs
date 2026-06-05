//! v7.12.4 — PL/pgSQL row-level trigger e2e suite.
//!
//! Tests build up from "trigger fires" → "NEW.col := … applied" →
//! "RETURN NULL skips" → "mailrs `search_vector` shape" against
//! the v7.12.3 GIN index. v7.12.5+ slices grow this file with
//! IF / DECLARE / embedded SQL coverage.

use spg_engine::Engine;
use spg_storage::Value;

fn eng() -> Engine {
    Engine::new()
}

fn ok(e: &mut Engine, sql: &str) {
    e.execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"));
}

fn rows(e: &mut Engine, sql: &str) -> Vec<spg_storage::Row> {
    let r = e
        .execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    match r {
        spg_engine::QueryResult::Rows { rows, .. } => rows,
        other => panic!("expected rows, got {other:?}"),
    }
}

fn first_value(e: &mut Engine, sql: &str) -> Value {
    rows(e, sql)
        .into_iter()
        .next()
        .map(|mut r| r.values.remove(0))
        .expect("at least one row")
}

#[test]
fn create_function_and_trigger_persist_in_catalog() {
    let mut e = eng();
    ok(&mut e, "CREATE TABLE t (id INT NOT NULL, v INT NOT NULL)");
    ok(
        &mut e,
        "CREATE FUNCTION noop() RETURNS TRIGGER LANGUAGE plpgsql AS $$ BEGIN RETURN NEW; END; $$",
    );
    ok(
        &mut e,
        "CREATE TRIGGER tg BEFORE INSERT ON t FOR EACH ROW EXECUTE FUNCTION noop()",
    );
    // Snapshot + restore round-trips functions + triggers.
    let bytes = e.snapshot();
    let cat = spg_storage::Catalog::deserialize(&bytes).unwrap();
    assert!(cat.functions().contains_key("noop"));
    assert!(cat.triggers().iter().any(|t| t.name == "tg"));
}

#[test]
fn before_insert_trigger_returns_new_unchanged_passes_row_through() {
    let mut e = eng();
    ok(&mut e, "CREATE TABLE t (id INT NOT NULL, v INT NOT NULL)");
    ok(
        &mut e,
        "CREATE FUNCTION noop() RETURNS TRIGGER LANGUAGE plpgsql AS $$ BEGIN RETURN NEW; END; $$",
    );
    ok(
        &mut e,
        "CREATE TRIGGER tg BEFORE INSERT ON t FOR EACH ROW EXECUTE FUNCTION noop()",
    );
    ok(&mut e, "INSERT INTO t VALUES (1, 10)");
    let v = first_value(&mut e, "SELECT v FROM t WHERE id = 1");
    assert_eq!(v, Value::Int(10));
}

#[test]
fn before_insert_trigger_returns_null_skips_the_row() {
    let mut e = eng();
    ok(&mut e, "CREATE TABLE t (id INT NOT NULL, v INT NOT NULL)");
    ok(
        &mut e,
        "CREATE FUNCTION blackhole() RETURNS TRIGGER LANGUAGE plpgsql AS $$ BEGIN RETURN NULL; END; $$",
    );
    ok(
        &mut e,
        "CREATE TRIGGER tg BEFORE INSERT ON t FOR EACH ROW EXECUTE FUNCTION blackhole()",
    );
    ok(&mut e, "INSERT INTO t VALUES (1, 10)");
    let rs = rows(&mut e, "SELECT id FROM t");
    assert!(
        rs.is_empty(),
        "trigger returned NULL, row should be skipped"
    );
}

#[test]
fn before_insert_trigger_rewrites_new_column() {
    // NEW.v := 999 — trigger overwrites the cell before write.
    let mut e = eng();
    ok(&mut e, "CREATE TABLE t (id INT NOT NULL, v INT NOT NULL)");
    ok(
        &mut e,
        "CREATE FUNCTION force_v() RETURNS TRIGGER LANGUAGE plpgsql AS $$ BEGIN NEW.v := 999; RETURN NEW; END; $$",
    );
    ok(
        &mut e,
        "CREATE TRIGGER tg BEFORE INSERT ON t FOR EACH ROW EXECUTE FUNCTION force_v()",
    );
    ok(&mut e, "INSERT INTO t VALUES (1, 10)");
    let v = first_value(&mut e, "SELECT v FROM t WHERE id = 1");
    // BEFORE trigger rewrote v from 10 to 999.
    assert_eq!(v, Value::Int(999));
}

#[test]
fn before_insert_trigger_mailrs_search_vector_shape() {
    // The mailrs G-CRIT-3 acceptance shape: NEW.search_vector is
    // populated from to_tsvector(NEW.<text columns>). Verifies
    // the end-to-end path (trigger fires, NEW.col := <function
    // call referencing NEW.col>, GIN-indexed column gets data).
    let mut e = eng();
    ok(
        &mut e,
        "CREATE TABLE messages (id INT NOT NULL, subject TEXT NOT NULL, sender TEXT NOT NULL, search_vector tsvector)",
    );
    ok(
        &mut e,
        "CREATE INDEX msg_sv_gin ON messages USING gin (search_vector)",
    );
    ok(
        &mut e,
        "CREATE FUNCTION update_sv() RETURNS TRIGGER LANGUAGE plpgsql AS $$
BEGIN
  NEW.search_vector := to_tsvector('simple', NEW.subject);
  RETURN NEW;
END;
$$",
    );
    ok(
        &mut e,
        "CREATE TRIGGER messages_sv BEFORE INSERT ON messages FOR EACH ROW EXECUTE FUNCTION update_sv()",
    );
    ok(
        &mut e,
        "INSERT INTO messages VALUES (1, 'the quick brown fox', 'alice@example.com', NULL)",
    );
    // Search via the GIN index — should match the row whose
    // search_vector got auto-populated by the trigger.
    let rs = rows(
        &mut e,
        "SELECT id FROM messages WHERE search_vector @@ to_tsquery('simple', 'fox')",
    );
    assert_eq!(rs.len(), 1, "expected fox to match the auto-populated row");
    assert!(matches!(rs[0].values[0], Value::Int(1)));
}

#[test]
fn after_trigger_cannot_assign_to_new() {
    let mut e = eng();
    ok(&mut e, "CREATE TABLE t (id INT NOT NULL, v INT NOT NULL)");
    ok(
        &mut e,
        "CREATE FUNCTION bad() RETURNS TRIGGER LANGUAGE plpgsql AS $$ BEGIN NEW.v := 1; RETURN NEW; END; $$",
    );
    ok(
        &mut e,
        "CREATE TRIGGER tg AFTER INSERT ON t FOR EACH ROW EXECUTE FUNCTION bad()",
    );
    let err = e
        .execute("INSERT INTO t VALUES (1, 10)")
        .expect_err("AFTER trigger assigning NEW must error");
    let msg = alloc_format(&err);
    assert!(
        msg.to_lowercase().contains("after") && msg.to_lowercase().contains("read-only"),
        "expected AFTER NEW read-only error, got {msg}"
    );
}

#[test]
fn drop_trigger_stops_firing() {
    let mut e = eng();
    ok(&mut e, "CREATE TABLE t (id INT NOT NULL, v INT NOT NULL)");
    ok(
        &mut e,
        "CREATE FUNCTION force_v() RETURNS TRIGGER LANGUAGE plpgsql AS $$ BEGIN NEW.v := 999; RETURN NEW; END; $$",
    );
    ok(
        &mut e,
        "CREATE TRIGGER tg BEFORE INSERT ON t FOR EACH ROW EXECUTE FUNCTION force_v()",
    );
    ok(&mut e, "INSERT INTO t VALUES (1, 10)");
    assert_eq!(
        first_value(&mut e, "SELECT v FROM t WHERE id = 1"),
        Value::Int(999)
    );
    // Drop the trigger; subsequent inserts should keep the
    // caller-provided value.
    ok(&mut e, "DROP TRIGGER tg ON t");
    ok(&mut e, "INSERT INTO t VALUES (2, 10)");
    assert_eq!(
        first_value(&mut e, "SELECT v FROM t WHERE id = 2"),
        Value::Int(10)
    );
}

fn alloc_format<T: core::fmt::Debug>(t: &T) -> String {
    format!("{t:?}")
}
