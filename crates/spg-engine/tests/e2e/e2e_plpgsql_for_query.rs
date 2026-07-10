//! v7.37.20 (20.5) — PL/pgSQL FOR <var> IN <SELECT> LOOP.

use spg_engine::Engine;

fn ddl(e: &mut Engine, sql: &str) {
    e.execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"));
}

#[test]
fn for_query_sum_matches_direct_sum() {
    let mut e = Engine::new();
    ddl(&mut e, "CREATE TABLE t (id INT)");
    ddl(&mut e, "INSERT INTO t VALUES (1), (2), (3), (4)");
    ddl(
        &mut e,
        "DO $$ DECLARE n INT := 0; row_id INT; \
         BEGIN FOR row_id IN SELECT id FROM t ORDER BY id LOOP \
             n := n + row_id; \
         END LOOP; ASSERT n = 10; END $$;",
    );
}

#[test]
fn for_query_empty_set_runs_zero_iterations() {
    let mut e = Engine::new();
    ddl(&mut e, "CREATE TABLE t (id INT)");
    ddl(
        &mut e,
        "DO $$ DECLARE n INT := 0; row_id INT; \
         BEGIN FOR row_id IN SELECT id FROM t LOOP \
             n := n + 1; \
         END LOOP; ASSERT n = 0; END $$;",
    );
}

#[test]
fn for_query_exit_when_breaks() {
    let mut e = Engine::new();
    ddl(&mut e, "CREATE TABLE t (id INT)");
    ddl(&mut e, "INSERT INTO t VALUES (1), (2), (3), (4), (5)");
    ddl(
        &mut e,
        "DO $$ DECLARE n INT := 0; row_id INT; \
         BEGIN FOR row_id IN SELECT id FROM t ORDER BY id LOOP \
             EXIT WHEN row_id = 3; \
             n := n + row_id; \
         END LOOP; ASSERT n = 3; END $$;",
    );
}
