//! v7.37.20 (20.16) — GET STACKED DIAGNOSTICS groundwork.
//! sqlerrm / sqlstate locals populated inside EXCEPTION handlers.

use spg_engine::Engine;

fn ddl(e: &mut Engine, sql: &str) {
    e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err:?}"));
}

#[test]
fn sqlerrm_populated_inside_exception_handler() {
    let mut e = Engine::new();
    ddl(
        &mut e,
        "DO $$ DECLARE err TEXT := ''; \
         BEGIN \
             RAISE EXCEPTION 'invariant X broken'; \
         EXCEPTION WHEN OTHERS THEN \
             err := sqlerrm; \
             ASSERT err = 'invariant X broken'; \
         END $$;",
    );
}

#[test]
fn sqlstate_placeholder_populated() {
    let mut e = Engine::new();
    ddl(
        &mut e,
        "DO $$ DECLARE code TEXT := ''; \
         BEGIN \
             RAISE EXCEPTION 'boom'; \
         EXCEPTION WHEN OTHERS THEN \
             code := sqlstate; \
             ASSERT code = 'P0001'; \
         END $$;",
    );
}

#[test]
fn sqlerrm_not_visible_before_exception() {
    // sqlerrm is only meaningful inside an EXCEPTION handler.
    // Referencing it before the raise + handler runs should
    // resolve to NULL — the exception hasn't been caught yet.
    let mut e = Engine::new();
    ddl(
        &mut e,
        "DO $$ DECLARE preview TEXT := 'sentinel'; \
         BEGIN \
             RAISE EXCEPTION 'x'; \
         EXCEPTION WHEN OTHERS THEN \
             ASSERT sqlerrm = 'x'; \
             ASSERT preview = 'sentinel'; \
         END $$;",
    );
}
