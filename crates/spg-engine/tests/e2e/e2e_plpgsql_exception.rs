//! v7.37.20 (20.10) — PL/pgSQL EXCEPTION WHEN handlers.

use spg_engine::Engine;

fn ddl(e: &mut Engine, sql: &str) {
    e.execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"));
}

#[test]
fn exception_others_swallows_raise_exception() {
    let mut e = Engine::new();
    ddl(
        &mut e,
        "DO $$ BEGIN \
             RAISE EXCEPTION 'boom'; \
         EXCEPTION WHEN OTHERS THEN \
             ASSERT TRUE; \
         END $$;",
    );
}

#[test]
fn exception_others_does_not_catch_an_assertion_failure() {
    // 8.0.3 — was `exception_others_catches_assertion_failure`. Measured on
    // PG 18.6: a failed ASSERT is P0004, and `WHEN OTHERS` deliberately
    // does not catch it — the block fails with `contradiction`. Only a
    // handler naming `assert_failure` catches it.
    let mut e = Engine::new();
    let err = e
        .execute(
            "DO $$ DECLARE recovered BOOL := FALSE; \
             BEGIN \
                 ASSERT 1 = 2, 'contradiction'; \
             EXCEPTION WHEN OTHERS THEN \
                 recovered := TRUE; \
             END $$;",
        )
        .expect_err("OTHERS does not catch P0004");
    assert!(format!("{err}").contains("contradiction"), "{err}");
    ddl(
        &mut e,
        "DO $$ BEGIN ASSERT 1 = 2, 'contradiction'; \
         EXCEPTION WHEN assert_failure THEN NULL; END $$;",
    );
}

#[test]
fn exception_without_matching_handler_propagates() {
    // Named condition 'unique_violation' shouldn't match the RAISE
    // message 'boom' (SPG substring model).
    let mut e = Engine::new();
    let err = e.execute(
        "DO $$ BEGIN \
             RAISE EXCEPTION 'boom'; \
         EXCEPTION WHEN unique_violation THEN NULL; \
         END $$;",
    );
    assert!(err.is_err(), "unmatched handler should re-raise");
}

#[test]
fn exception_or_conditions_share_body() {
    // 8.0.3 — this used `WHEN foo OR divergent OR bar` to catch
    // `RAISE EXCEPTION 'divergent'`, which only worked because a condition
    // was matched against the RAISE MESSAGE by substring — SPG's invention.
    // PostgreSQL 18.6 refuses that block before it runs:
    // `unrecognized exception condition "foo"` (42704). An OR list of real
    // condition names shares one body, which is what this pin is for.
    let mut e = Engine::new();
    ddl(
        &mut e,
        "DO $$ BEGIN \
             RAISE EXCEPTION 'divergent'; \
         EXCEPTION WHEN division_by_zero OR raise_exception OR unique_violation THEN ASSERT TRUE; \
         END $$;",
    );
    let err = e
        .execute(
            "DO $$ BEGIN RAISE EXCEPTION 'divergent'; \
             EXCEPTION WHEN foo OR divergent OR bar THEN ASSERT TRUE; END $$;",
        )
        .expect_err("PG refuses an unknown condition name");
    assert!(
        format!("{err}").contains("unrecognized exception condition \"foo\""),
        "{err}"
    );
}

#[test]
fn body_that_completes_never_triggers_handler() {
    let mut e = Engine::new();
    ddl(
        &mut e,
        "DO $$ DECLARE ran_handler BOOL := FALSE; \
         BEGIN ASSERT TRUE; \
         EXCEPTION WHEN OTHERS THEN ran_handler := TRUE; \
         END $$;",
    );
}
