//! v7.37.20 (20.2) — PL/pgSQL CONTINUE [WHEN] statement.

use spg_engine::Engine;

fn ddl(e: &mut Engine, sql: &str) {
    e.execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"));
}

#[test]
fn continue_when_skips_odd_iters() {
    let mut e = Engine::new();
    ddl(
        &mut e,
        "DO $$ DECLARE n INT := 0; i INT; \
         BEGIN FOR i IN 1..10 LOOP \
             CONTINUE WHEN i % 2 = 1; \
             n := n + i; \
         END LOOP; ASSERT n = 30; END $$;",
    );
}

#[test]
fn unconditional_continue_body_no_op_bump() {
    // Bare CONTINUE at the top of the body means every iteration is
    // effectively empty; loop still runs its full range.
    let mut e = Engine::new();
    ddl(
        &mut e,
        "DO $$ DECLARE n INT := 0; i INT; \
         BEGIN FOR i IN 1..5 LOOP \
             CONTINUE; \
             n := n + i; \
         END LOOP; ASSERT n = 0; END $$;",
    );
}

#[test]
fn continue_when_false_falls_through() {
    let mut e = Engine::new();
    ddl(
        &mut e,
        "DO $$ DECLARE n INT := 0; i INT; \
         BEGIN FOR i IN 1..5 LOOP \
             CONTINUE WHEN 1 = 2; \
             n := n + i; \
         END LOOP; ASSERT n = 15; END $$;",
    );
}
