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
fn exception_others_catches_assertion_failure() {
    let mut e = Engine::new();
    ddl(
        &mut e,
        "DO $$ DECLARE recovered BOOL := FALSE; \
         BEGIN \
             ASSERT 1 = 2, 'contradiction'; \
         EXCEPTION WHEN OTHERS THEN \
             recovered := TRUE; \
         END $$;",
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
    let mut e = Engine::new();
    ddl(
        &mut e,
        "DO $$ BEGIN \
             RAISE EXCEPTION 'divergent'; \
         EXCEPTION WHEN foo OR divergent OR bar THEN ASSERT TRUE; \
         END $$;",
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
