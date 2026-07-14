//! v7.37.20 (20.11) — PL/pgSQL RETURN QUERY / RETURN QUERY EXECUTE
//! desugared to PERFORM-equivalent inside DO block context.

use spg_engine::Engine;

fn ddl(e: &mut Engine, sql: &str) {
    e.execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"));
}

#[test]
fn return_query_select_runs_for_side_effects_in_do() {
    let mut e = Engine::new();
    ddl(&mut e, "CREATE TABLE t (id INT)");
    ddl(&mut e, "INSERT INTO t VALUES (1)");
    // v7.39 (read01 round 66) — RETURN QUERY is a REAL statement now: it appends
    // to the set a SETOF function is building. A DO block returns nothing, so it
    // has nowhere to append — and PG says exactly that. This test used to assert
    // the old desugaring (run the SELECT, DISCARD its rows, complete cleanly),
    // which in a set-returning function meant throwing the whole answer away.
    let msg = format!(
        "{}",
        e.execute("DO $$ BEGIN RETURN QUERY SELECT id FROM t; END $$;")
            .unwrap_err()
    );
    assert!(
        msg.contains("cannot use RETURN QUERY in a non-SETOF function"),
        "{msg}"
    );
}

#[test]
fn return_query_execute_runs_dynamic_sql_in_do() {
    let mut e = Engine::new();
    ddl(&mut e, "CREATE TABLE t (id INT)");
    ddl(&mut e, "INSERT INTO t VALUES (7)");
    // v7.39 (read01 round 68) — the dynamic twin of RETURN QUERY joins it: its
    // rows go to the SET a set-returning function is building, instead of being
    // run and discarded. A DO block has no set, so PG rejects it there — and now
    // so does SPG. (This test used to assert the discard path.)
    let msg = format!(
        "{}",
        e.execute("DO $$ BEGIN RETURN QUERY EXECUTE 'SELECT id FROM t'; END $$;")
            .unwrap_err()
    );
    assert!(
        msg.contains("cannot use RETURN QUERY in a non-SETOF function"),
        "{msg}"
    );
}

#[test]
fn return_null_still_works() {
    let mut e = Engine::new();
    ddl(&mut e, "DO $$ BEGIN RETURN NULL; END $$;");
}
