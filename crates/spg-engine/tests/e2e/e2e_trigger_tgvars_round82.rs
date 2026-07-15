//! v7.39 (read01 round 82) — a sweep of the trigger / generated-column surface.
//! Two real bugs, both silent, and both about ORDER OF OPERATIONS.
//!
//! 1. The TG_* magic variables did not exist. `TG_OP`, `TG_WHEN`, `TG_LEVEL`,
//!    `TG_NAME`, `TG_TABLE_NAME`, `TG_NARGS` are how a trigger function knows
//!    what fired it — an audit or dispatch trigger reads `TG_OP` on its first
//!    line. SPG bound none, so every such function died on
//!    "column tg_op does not exist". The interpreter had NEW / OLD but not the
//!    trigger's own identity.
//!
//! 2. A stored generated column was computed BEFORE the BEFORE trigger, not
//!    after. PG's order is: BEFORE trigger runs (and may rewrite NEW) → generated
//!    columns evaluate → row is written. SPG evaluated them first, so
//!    `w GENERATED ALWAYS AS (v*2)` held the doubling of the ORIGINAL v when a
//!    BEFORE trigger changed NEW.v. The row on disk was internally inconsistent —
//!    `w != v*2` — which no error ever flags.
//!
//! Also: CREATE CONSTRAINT TRIGGER now parses (fired as a plain AFTER trigger,
//! correct for every non-deferred use).

use spg_engine::{Engine, QueryResult};

fn ok(e: &mut Engine, sql: &str) {
    e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err:?}"));
}

fn joined(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap_or_else(|x| panic!("{sql}: {x:?}")) {
        QueryResult::Rows { rows, .. } => rows
            .iter()
            .map(|r| {
                r.values
                    .iter()
                    .map(spg_engine::eval::value_to_text)
                    .collect::<Vec<_>>()
                    .join("|")
            })
            .collect::<Vec<_>>()
            .join(","),
        other => panic!("{sql}: {other:?}"),
    }
}

#[test]
fn a_tg_variables_are_visible() {
    let mut e = Engine::new();
    ok(&mut e, "CREATE TABLE t (id int, v int)");
    ok(&mut e, "CREATE TABLE log (msg text)");
    ok(
        &mut e,
        "CREATE FUNCTION f() RETURNS trigger AS $$ BEGIN \
         INSERT INTO log VALUES (TG_OP||'/'||TG_WHEN||'/'||TG_LEVEL||'/'||TG_NAME||'/'||\
         TG_TABLE_NAME||'/'||TG_NARGS::text); RETURN NEW; END; $$ LANGUAGE plpgsql",
    );
    ok(&mut e, "CREATE TRIGGER tg1 BEFORE INSERT ON t FOR EACH ROW EXECUTE FUNCTION f()");
    ok(&mut e, "INSERT INTO t VALUES (1,10)");
    assert_eq!(joined(&mut e, "SELECT msg FROM log"), "INSERT/BEFORE/ROW/tg1/t/0");
}

#[test]
fn b_tg_op_dispatch_on_all_three_events() {
    let mut e = Engine::new();
    ok(&mut e, "CREATE TABLE t (id int PRIMARY KEY, v int)");
    ok(&mut e, "CREATE TABLE aud (op text, oid int, nv int)");
    ok(
        &mut e,
        "CREATE FUNCTION trg() RETURNS trigger AS $$ BEGIN \
         IF TG_OP='DELETE' THEN INSERT INTO aud VALUES (TG_OP, OLD.id, OLD.v); RETURN OLD; \
         ELSE INSERT INTO aud VALUES (TG_OP, NEW.id, NEW.v); RETURN NEW; END IF; END; $$ \
         LANGUAGE plpgsql",
    );
    ok(
        &mut e,
        "CREATE TRIGGER trg_a AFTER INSERT OR UPDATE OR DELETE ON t \
         FOR EACH ROW EXECUTE FUNCTION trg()",
    );
    ok(&mut e, "INSERT INTO t VALUES (1,10),(2,20)");
    ok(&mut e, "UPDATE t SET v=50 WHERE id=2");
    ok(&mut e, "DELETE FROM t WHERE id=1");
    assert_eq!(
        joined(
            &mut e,
            "SELECT op||'/'||oid||'/'||coalesce(nv::text,'-') FROM aud ORDER BY oid, op"
        ),
        "DELETE/1/10,INSERT/1/10,INSERT/2/20,UPDATE/2/50"
    );
}

#[test]
fn c_generated_column_recomputes_after_before_trigger() {
    let mut e = Engine::new();
    ok(
        &mut e,
        "CREATE TABLE t (id int, v int, w int GENERATED ALWAYS AS (v*2) STORED)",
    );
    ok(
        &mut e,
        "CREATE FUNCTION bt() RETURNS trigger AS $$ BEGIN NEW.v := NEW.v + 1; RETURN NEW; END; $$ \
         LANGUAGE plpgsql",
    );
    ok(&mut e, "CREATE TRIGGER trg_bi BEFORE INSERT ON t FOR EACH ROW EXECUTE FUNCTION bt()");
    ok(&mut e, "INSERT INTO t (id,v) VALUES (1,100)");
    // trigger sets v to 101, THEN w = v*2 = 202. Not 200 (the pre-trigger 100*2).
    assert_eq!(joined(&mut e, "SELECT v||'/'||w FROM t WHERE id=1"), "101/202");

    // Same for UPDATE.
    ok(
        &mut e,
        "CREATE FUNCTION bu() RETURNS trigger AS $$ BEGIN NEW.v := NEW.v + 5; RETURN NEW; END; $$ \
         LANGUAGE plpgsql",
    );
    ok(&mut e, "CREATE TRIGGER trg_bu BEFORE UPDATE ON t FOR EACH ROW EXECUTE FUNCTION bu()");
    ok(&mut e, "UPDATE t SET v=200 WHERE id=1");
    // trigger sets v to 205, w = 410.
    assert_eq!(joined(&mut e, "SELECT v||'/'||w FROM t WHERE id=1"), "205/410");
}

#[test]
fn d_constraint_trigger_parses_and_fires() {
    let mut e = Engine::new();
    ok(&mut e, "CREATE TABLE t (id int, v int)");
    ok(&mut e, "CREATE TABLE log (n int)");
    ok(
        &mut e,
        "CREATE FUNCTION f() RETURNS trigger AS $$ BEGIN INSERT INTO log VALUES (NEW.id); \
         RETURN NEW; END; $$ LANGUAGE plpgsql",
    );
    ok(
        &mut e,
        "CREATE CONSTRAINT TRIGGER ct AFTER INSERT ON t DEFERRABLE INITIALLY DEFERRED \
         FOR EACH ROW EXECUTE FUNCTION f()",
    );
    ok(&mut e, "INSERT INTO t VALUES (7,10)");
    assert_eq!(joined(&mut e, "SELECT n FROM log"), "7");
}
