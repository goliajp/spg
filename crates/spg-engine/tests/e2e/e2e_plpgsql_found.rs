//! v7.37.20 (20.15) — PL/pgSQL FOUND special variable set after
//! SELECT INTO returns.

use spg_engine::Engine;

fn ddl(e: &mut Engine, sql: &str) {
    e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err:?}"));
}

#[test]
fn found_true_after_select_into_matches() {
    let mut e = Engine::new();
    ddl(&mut e, "CREATE TABLE t (id INT)");
    ddl(&mut e, "INSERT INTO t VALUES (42)");
    ddl(
        &mut e,
        "DO $$ DECLARE x INT; found BOOL := FALSE; \
         BEGIN SELECT id INTO x FROM t WHERE id = 42; \
         ASSERT found = TRUE; END $$;",
    );
}

#[test]
fn found_false_after_select_into_misses() {
    let mut e = Engine::new();
    ddl(&mut e, "CREATE TABLE t (id INT)");
    ddl(&mut e, "INSERT INTO t VALUES (1)");
    ddl(
        &mut e,
        "DO $$ DECLARE x INT; found BOOL := TRUE; \
         BEGIN SELECT id INTO x FROM t WHERE id = 999; \
         ASSERT found = FALSE; END $$;",
    );
}

#[test]
fn found_switches_across_multiple_select_into() {
    let mut e = Engine::new();
    ddl(&mut e, "CREATE TABLE t (id INT)");
    ddl(&mut e, "INSERT INTO t VALUES (10)");
    ddl(
        &mut e,
        "DO $$ DECLARE x INT; found BOOL := FALSE; \
         BEGIN \
             SELECT id INTO x FROM t WHERE id = 10; \
             ASSERT found = TRUE; \
             SELECT id INTO x FROM t WHERE id = 999; \
             ASSERT found = FALSE; \
         END $$;",
    );
}
