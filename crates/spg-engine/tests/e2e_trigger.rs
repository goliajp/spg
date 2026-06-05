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

// --- v7.12.5: UPDATE + DELETE trigger hooks ---

#[test]
fn before_update_trigger_can_rewrite_new_column() {
    // BEFORE UPDATE sees NEW (post-update candidate) + OLD
    // (pre-update). Trigger rewrites NEW.v to a sentinel value.
    let mut e = eng();
    ok(&mut e, "CREATE TABLE t (id INT NOT NULL, v INT NOT NULL)");
    ok(&mut e, "INSERT INTO t VALUES (1, 10)");
    ok(
        &mut e,
        "CREATE FUNCTION force_v() RETURNS TRIGGER LANGUAGE plpgsql AS $$ BEGIN NEW.v := 777; RETURN NEW; END; $$",
    );
    ok(
        &mut e,
        "CREATE TRIGGER tg BEFORE UPDATE ON t FOR EACH ROW EXECUTE FUNCTION force_v()",
    );
    ok(&mut e, "UPDATE t SET v = 99 WHERE id = 1");
    // The user's UPDATE asked for v=99 but the BEFORE trigger
    // rewrote it to 777 before the write landed.
    assert_eq!(
        first_value(&mut e, "SELECT v FROM t WHERE id = 1"),
        Value::Int(777)
    );
}

#[test]
fn before_update_trigger_can_skip_via_return_null() {
    let mut e = eng();
    ok(&mut e, "CREATE TABLE t (id INT NOT NULL, v INT NOT NULL)");
    ok(&mut e, "INSERT INTO t VALUES (1, 10)");
    ok(
        &mut e,
        "CREATE FUNCTION veto() RETURNS TRIGGER LANGUAGE plpgsql AS $$ BEGIN RETURN NULL; END; $$",
    );
    ok(
        &mut e,
        "CREATE TRIGGER tg BEFORE UPDATE ON t FOR EACH ROW EXECUTE FUNCTION veto()",
    );
    ok(&mut e, "UPDATE t SET v = 99 WHERE id = 1");
    // The trigger returned NULL — UPDATE skipped, v stays 10.
    assert_eq!(
        first_value(&mut e, "SELECT v FROM t WHERE id = 1"),
        Value::Int(10)
    );
}

#[test]
fn before_update_trigger_sees_old_row() {
    // Trigger writes NEW.v := OLD.v + 1 — a typical "increment
    // on update" pattern.
    let mut e = eng();
    ok(&mut e, "CREATE TABLE t (id INT NOT NULL, v INT NOT NULL)");
    ok(&mut e, "INSERT INTO t VALUES (1, 10)");
    ok(
        &mut e,
        "CREATE FUNCTION bump() RETURNS TRIGGER LANGUAGE plpgsql AS $$ BEGIN NEW.v := OLD.v + 1; RETURN NEW; END; $$",
    );
    ok(
        &mut e,
        "CREATE TRIGGER tg BEFORE UPDATE ON t FOR EACH ROW EXECUTE FUNCTION bump()",
    );
    ok(&mut e, "UPDATE t SET v = 999 WHERE id = 1");
    // OLD.v was 10, NEW.v becomes 11 (regardless of what user
    // SET told us to write).
    assert_eq!(
        first_value(&mut e, "SELECT v FROM t WHERE id = 1"),
        Value::Int(11)
    );
}

#[test]
fn before_delete_trigger_can_veto_individual_rows() {
    let mut e = eng();
    ok(
        &mut e,
        "CREATE TABLE t (id INT NOT NULL, keep INT NOT NULL)",
    );
    ok(&mut e, "INSERT INTO t VALUES (1, 0)");
    ok(&mut e, "INSERT INTO t VALUES (2, 1)");
    ok(&mut e, "INSERT INTO t VALUES (3, 0)");
    // Trigger: skip rows where OLD.keep = 1.
    ok(
        &mut e,
        "CREATE FUNCTION guard() RETURNS TRIGGER LANGUAGE plpgsql AS $$
BEGIN
  NEW.keep := OLD.keep;
  RETURN NEW;
END;
$$",
    );
    // The trigger above runs as BEFORE UPDATE (no DELETE skip);
    // for the DELETE-skip semantics build a dedicated trigger that
    // returns NULL when OLD.keep = 1. v7.12.5 doesn't yet have IF
    // in PL/pgSQL, so this test instead verifies the easier shape:
    // a BEFORE DELETE that always returns NULL skips ALL deletes.
    ok(
        &mut e,
        "CREATE FUNCTION blackhole_del() RETURNS TRIGGER LANGUAGE plpgsql AS $$ BEGIN RETURN NULL; END; $$",
    );
    ok(
        &mut e,
        "CREATE TRIGGER tg_del BEFORE DELETE ON t FOR EACH ROW EXECUTE FUNCTION blackhole_del()",
    );
    let r = e
        .execute("DELETE FROM t WHERE id IN (1, 2, 3)")
        .expect("delete should succeed (just no-op)");
    if let spg_engine::QueryResult::CommandOk { affected, .. } = r {
        assert_eq!(
            affected, 0,
            "all 3 rows skipped via BEFORE DELETE RETURN NULL"
        );
    }
    // All rows still present.
    let count = first_value(&mut e, "SELECT count(*) FROM t");
    assert_eq!(count, Value::BigInt(3));
}

#[test]
fn after_update_trigger_cannot_assign_to_new() {
    let mut e = eng();
    ok(&mut e, "CREATE TABLE t (id INT NOT NULL, v INT NOT NULL)");
    ok(&mut e, "INSERT INTO t VALUES (1, 10)");
    ok(
        &mut e,
        "CREATE FUNCTION bad_after() RETURNS TRIGGER LANGUAGE plpgsql AS $$ BEGIN NEW.v := 1; RETURN NEW; END; $$",
    );
    ok(
        &mut e,
        "CREATE TRIGGER tg AFTER UPDATE ON t FOR EACH ROW EXECUTE FUNCTION bad_after()",
    );
    let err = e
        .execute("UPDATE t SET v = 99 WHERE id = 1")
        .expect_err("AFTER UPDATE writing NEW must error");
    let msg = alloc_format(&err);
    assert!(
        msg.to_lowercase().contains("after") && msg.to_lowercase().contains("read-only"),
        "expected AFTER NEW read-only error, got {msg}"
    );
}

// --- v7.12.6: IF / ELSIF / ELSE control flow ---

#[test]
fn before_insert_trigger_if_then_rewrites_only_when_condition_true() {
    let mut e = eng();
    ok(&mut e, "CREATE TABLE t (id INT NOT NULL, v INT NOT NULL)");
    // If v == 10 → rewrite to 999. Otherwise pass-through.
    ok(
        &mut e,
        "CREATE FUNCTION gated() RETURNS TRIGGER LANGUAGE plpgsql AS $$
BEGIN
  IF NEW.v = 10 THEN
    NEW.v := 999;
  END IF;
  RETURN NEW;
END;
$$",
    );
    ok(
        &mut e,
        "CREATE TRIGGER tg BEFORE INSERT ON t FOR EACH ROW EXECUTE FUNCTION gated()",
    );
    ok(&mut e, "INSERT INTO t VALUES (1, 10)");
    ok(&mut e, "INSERT INTO t VALUES (2, 11)");
    assert_eq!(
        first_value(&mut e, "SELECT v FROM t WHERE id = 1"),
        Value::Int(999),
        "row matching IF predicate gets rewritten"
    );
    assert_eq!(
        first_value(&mut e, "SELECT v FROM t WHERE id = 2"),
        Value::Int(11),
        "row not matching IF predicate passes through"
    );
}

#[test]
fn before_insert_trigger_if_elsif_else_chain() {
    let mut e = eng();
    ok(&mut e, "CREATE TABLE t (id INT NOT NULL, v INT NOT NULL)");
    ok(
        &mut e,
        "CREATE FUNCTION grade() RETURNS TRIGGER LANGUAGE plpgsql AS $$
BEGIN
  IF NEW.v < 60 THEN
    NEW.v := 0;
  ELSIF NEW.v < 90 THEN
    NEW.v := 50;
  ELSE
    NEW.v := 100;
  END IF;
  RETURN NEW;
END;
$$",
    );
    ok(
        &mut e,
        "CREATE TRIGGER tg BEFORE INSERT ON t FOR EACH ROW EXECUTE FUNCTION grade()",
    );
    ok(&mut e, "INSERT INTO t VALUES (1, 50)");
    ok(&mut e, "INSERT INTO t VALUES (2, 75)");
    ok(&mut e, "INSERT INTO t VALUES (3, 95)");
    assert_eq!(
        first_value(&mut e, "SELECT v FROM t WHERE id = 1"),
        Value::Int(0)
    );
    assert_eq!(
        first_value(&mut e, "SELECT v FROM t WHERE id = 2"),
        Value::Int(50)
    );
    assert_eq!(
        first_value(&mut e, "SELECT v FROM t WHERE id = 3"),
        Value::Int(100)
    );
}

#[test]
fn before_insert_trigger_if_can_short_circuit_via_return_null() {
    let mut e = eng();
    ok(&mut e, "CREATE TABLE t (id INT NOT NULL, v INT NOT NULL)");
    ok(
        &mut e,
        "CREATE FUNCTION pos_only() RETURNS TRIGGER LANGUAGE plpgsql AS $$
BEGIN
  IF NEW.v < 0 THEN
    RETURN NULL;
  END IF;
  RETURN NEW;
END;
$$",
    );
    ok(
        &mut e,
        "CREATE TRIGGER tg BEFORE INSERT ON t FOR EACH ROW EXECUTE FUNCTION pos_only()",
    );
    ok(&mut e, "INSERT INTO t VALUES (1, 5)");
    ok(&mut e, "INSERT INTO t VALUES (2, -3)");
    let count = first_value(&mut e, "SELECT count(*) FROM t");
    assert_eq!(count, Value::BigInt(1));
}

// --- v7.12.6: DECLARE local variables ---

#[test]
fn before_insert_trigger_declare_and_use_local_var() {
    let mut e = eng();
    ok(&mut e, "CREATE TABLE t (id INT NOT NULL, v INT NOT NULL)");
    ok(
        &mut e,
        "CREATE FUNCTION doubled() RETURNS TRIGGER LANGUAGE plpgsql AS $$
DECLARE
  scaled INT := NEW.v * 2;
BEGIN
  NEW.v := scaled;
  RETURN NEW;
END;
$$",
    );
    ok(
        &mut e,
        "CREATE TRIGGER tg BEFORE INSERT ON t FOR EACH ROW EXECUTE FUNCTION doubled()",
    );
    ok(&mut e, "INSERT INTO t VALUES (1, 7)");
    assert_eq!(
        first_value(&mut e, "SELECT v FROM t WHERE id = 1"),
        Value::Int(14)
    );
}

#[test]
fn before_insert_trigger_declare_chain_and_assign() {
    // Earlier DECLAREs are in scope for later DECLAREs.
    let mut e = eng();
    ok(&mut e, "CREATE TABLE t (id INT NOT NULL, v INT NOT NULL)");
    ok(
        &mut e,
        "CREATE FUNCTION chained() RETURNS TRIGGER LANGUAGE plpgsql AS $$
DECLARE
  a INT := NEW.v + 1;
  b INT := a * 10;
BEGIN
  NEW.v := b;
  RETURN NEW;
END;
$$",
    );
    ok(
        &mut e,
        "CREATE TRIGGER tg BEFORE INSERT ON t FOR EACH ROW EXECUTE FUNCTION chained()",
    );
    ok(&mut e, "INSERT INTO t VALUES (1, 4)");
    assert_eq!(
        first_value(&mut e, "SELECT v FROM t WHERE id = 1"),
        Value::Int(50)
    );
}

// --- v7.12.6: RAISE NOTICE / EXCEPTION ---

#[test]
fn before_insert_trigger_raise_notice_does_not_block_write() {
    let mut e = eng();
    ok(&mut e, "CREATE TABLE t (id INT NOT NULL, v INT NOT NULL)");
    ok(
        &mut e,
        "CREATE FUNCTION log_it() RETURNS TRIGGER LANGUAGE plpgsql AS $$
BEGIN
  RAISE NOTICE 'inserting row %', NEW.id;
  RETURN NEW;
END;
$$",
    );
    ok(
        &mut e,
        "CREATE TRIGGER tg BEFORE INSERT ON t FOR EACH ROW EXECUTE FUNCTION log_it()",
    );
    ok(&mut e, "INSERT INTO t VALUES (1, 42)");
    assert_eq!(
        first_value(&mut e, "SELECT v FROM t WHERE id = 1"),
        Value::Int(42)
    );
}

#[test]
fn before_insert_trigger_raise_exception_blocks_write_and_propagates_message() {
    let mut e = eng();
    ok(&mut e, "CREATE TABLE t (id INT NOT NULL, v INT NOT NULL)");
    ok(
        &mut e,
        "CREATE FUNCTION reject_neg() RETURNS TRIGGER LANGUAGE plpgsql AS $$
BEGIN
  IF NEW.v < 0 THEN
    RAISE EXCEPTION 'negative value % rejected', NEW.v;
  END IF;
  RETURN NEW;
END;
$$",
    );
    ok(
        &mut e,
        "CREATE TRIGGER tg BEFORE INSERT ON t FOR EACH ROW EXECUTE FUNCTION reject_neg()",
    );
    // Positive value goes in.
    ok(&mut e, "INSERT INTO t VALUES (1, 5)");
    // Negative value triggers RAISE EXCEPTION → propagates as
    // engine error.
    let err = e
        .execute("INSERT INTO t VALUES (2, -10)")
        .expect_err("RAISE EXCEPTION must abort the insert");
    let msg = alloc_format(&err);
    assert!(
        msg.contains("negative value") && msg.contains("-10"),
        "RAISE EXCEPTION message + arg substitution should propagate: {msg}"
    );
    // Row 2 didn't land.
    let count = first_value(&mut e, "SELECT count(*) FROM t");
    assert_eq!(count, Value::BigInt(1));
}

#[test]
fn mailrs_update_search_vector_on_subject_change() {
    // mailrs G-CRIT-3 UPDATE-shape acceptance: when a message's
    // `subject` changes, `search_vector` must auto-recompute via
    // the BEFORE UPDATE trigger and the GIN index must pick up
    // the new lexemes. Mirrors the v7.12.4 INSERT-shape test.
    let mut e = eng();
    ok(
        &mut e,
        "CREATE TABLE messages (id INT NOT NULL, subject TEXT NOT NULL, search_vector tsvector)",
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
    // Fires on both INSERT and UPDATE — covers the full mailrs
    // lifecycle, not just the v7.12.4 INSERT branch.
    ok(
        &mut e,
        "CREATE TRIGGER messages_sv BEFORE INSERT OR UPDATE ON messages FOR EACH ROW EXECUTE FUNCTION update_sv()",
    );
    ok(
        &mut e,
        "INSERT INTO messages VALUES (1, 'the quick brown fox', NULL)",
    );
    let rs = rows(
        &mut e,
        "SELECT id FROM messages WHERE search_vector @@ to_tsquery('simple', 'fox')",
    );
    assert_eq!(rs.len(), 1, "initial INSERT path: fox matches");
    // Subject change → trigger re-runs to_tsvector against the
    // new subject → GIN index re-indexes via rebuild_indices.
    ok(
        &mut e,
        "UPDATE messages SET subject = 'lazy cats sleep' WHERE id = 1",
    );
    let rs_fox = rows(
        &mut e,
        "SELECT id FROM messages WHERE search_vector @@ to_tsquery('simple', 'fox')",
    );
    assert!(
        rs_fox.is_empty(),
        "post-UPDATE: fox should NOT match — subject changed and trigger should have rewritten search_vector"
    );
    let rs_cats = rows(
        &mut e,
        "SELECT id FROM messages WHERE search_vector @@ to_tsquery('simple', 'cats')",
    );
    assert_eq!(
        rs_cats.len(),
        1,
        "post-UPDATE: cats should match the new subject's lexemes"
    );
}
