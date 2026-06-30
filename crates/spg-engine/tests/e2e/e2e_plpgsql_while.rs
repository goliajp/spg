//! v7.37.20 (20.3) — PL/pgSQL WHILE LOOP.

use spg_engine::Engine;

fn ddl(e: &mut Engine, sql: &str) {
    e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err:?}"));
}

#[test]
fn while_false_body_runs_zero_times() {
    let mut e = Engine::new();
    ddl(
        &mut e,
        "DO $$ DECLARE n INT := 0; BEGIN WHILE 1 = 2 LOOP n := n + 1; END LOOP; \
         ASSERT n = 0; END $$;",
    );
}

#[test]
fn while_counts_up_until_condition_false() {
    let mut e = Engine::new();
    ddl(
        &mut e,
        "DO $$ DECLARE n INT := 0; BEGIN WHILE n < 5 LOOP n := n + 1; END LOOP; \
         ASSERT n = 5; END $$;",
    );
}

#[test]
fn while_assert_inside_body_propagates_failure() {
    let mut e = Engine::new();
    let err = e.execute(
        "DO $$ DECLARE n INT := 0; BEGIN WHILE n < 5 LOOP n := n + 1; \
         ASSERT n < 3; END LOOP; END $$;",
    );
    assert!(err.is_err(), "ASSERT inside WHILE should propagate");
}
