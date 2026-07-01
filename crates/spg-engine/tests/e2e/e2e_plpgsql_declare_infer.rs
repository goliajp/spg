//! v7.37.20 (20.7) — PL/pgSQL DECLARE with type inference from
//! the default expression (no explicit type token).

use spg_engine::Engine;

fn ddl(e: &mut Engine, sql: &str) {
    e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err:?}"));
}

#[test]
fn declare_infer_int_from_default() {
    let mut e = Engine::new();
    ddl(
        &mut e,
        "DO $$ DECLARE x := 42; BEGIN ASSERT x = 42; END $$;",
    );
}

#[test]
fn declare_infer_text_from_default() {
    let mut e = Engine::new();
    ddl(
        &mut e,
        "DO $$ DECLARE msg := 'hello'; \
         BEGIN ASSERT msg = 'hello'; END $$;",
    );
}

#[test]
fn declare_infer_from_computed_expression() {
    let mut e = Engine::new();
    ddl(
        &mut e,
        "DO $$ DECLARE n := 10 + 5; \
         BEGIN ASSERT n = 15; END $$;",
    );
}

#[test]
fn declare_explicit_type_still_works() {
    let mut e = Engine::new();
    ddl(
        &mut e,
        "DO $$ DECLARE x INT := 100; \
         BEGIN ASSERT x = 100; END $$;",
    );
}
