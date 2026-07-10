//! v7.37.20 (20.2) — bare LOOP + EXIT [WHEN] statement.

use spg_engine::Engine;

fn ddl(e: &mut Engine, sql: &str) {
    e.execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"));
}

#[test]
fn loop_with_exit_when_breaks_on_condition() {
    let mut e = Engine::new();
    ddl(
        &mut e,
        "DO $$ DECLARE n INT := 0; \
         BEGIN LOOP n := n + 1; EXIT WHEN n = 5; END LOOP; \
         ASSERT n = 5; END $$;",
    );
}

#[test]
fn loop_with_unconditional_exit_breaks_immediately() {
    let mut e = Engine::new();
    ddl(
        &mut e,
        "DO $$ DECLARE n INT := 0; \
         BEGIN LOOP n := n + 1; EXIT; END LOOP; \
         ASSERT n = 1; END $$;",
    );
}

#[test]
fn loop_exit_when_false_continues() {
    let mut e = Engine::new();
    ddl(
        &mut e,
        "DO $$ DECLARE n INT := 0; \
         BEGIN LOOP n := n + 1; EXIT WHEN n >= 10; END LOOP; \
         ASSERT n = 10; END $$;",
    );
}

#[test]
fn loop_exit_inside_if_still_breaks() {
    let mut e = Engine::new();
    ddl(
        &mut e,
        "DO $$ DECLARE n INT := 0; \
         BEGIN LOOP n := n + 1; \
             IF n = 3 THEN EXIT; END IF; \
         END LOOP; ASSERT n = 3; END $$;",
    );
}
