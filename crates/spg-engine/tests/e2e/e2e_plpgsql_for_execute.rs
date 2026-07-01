//! v7.37.20 (20.6) — PL/pgSQL FOR <var> IN EXECUTE <expr> LOOP.

use spg_engine::Engine;

fn ddl(e: &mut Engine, sql: &str) {
    e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err:?}"));
}

#[test]
fn for_execute_iterates_computed_query() {
    let mut e = Engine::new();
    ddl(&mut e, "CREATE TABLE t (id INT)");
    ddl(&mut e, "INSERT INTO t VALUES (1), (2), (3)");
    ddl(
        &mut e,
        "DO $$ DECLARE n INT := 0; row_id INT; tab TEXT := 't'; \
         BEGIN FOR row_id IN EXECUTE 'SELECT id FROM ' || tab || ' ORDER BY id' LOOP \
             n := n + row_id; \
         END LOOP; ASSERT n = 6; END $$;",
    );
}

#[test]
fn for_execute_empty_query_zero_iterations() {
    let mut e = Engine::new();
    ddl(&mut e, "CREATE TABLE t (id INT)");
    ddl(
        &mut e,
        "DO $$ DECLARE n INT := 0; row_id INT; \
         BEGIN FOR row_id IN EXECUTE 'SELECT id FROM t' LOOP \
             n := n + 1; \
         END LOOP; ASSERT n = 0; END $$;",
    );
}

#[test]
fn for_execute_continue_when_skips() {
    let mut e = Engine::new();
    ddl(&mut e, "CREATE TABLE t (id INT)");
    ddl(&mut e, "INSERT INTO t VALUES (1), (2), (3), (4), (5)");
    ddl(
        &mut e,
        "DO $$ DECLARE n INT := 0; row_id INT; \
         BEGIN FOR row_id IN EXECUTE 'SELECT id FROM t ORDER BY id' LOOP \
             CONTINUE WHEN row_id % 2 = 1; \
             n := n + row_id; \
         END LOOP; ASSERT n = 6; END $$;",
    );
}
