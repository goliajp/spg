//! v7.37.20 (20.8) — DECLARE with %TYPE / %ROWTYPE parse-accept.

use spg_engine::Engine;

fn ddl(e: &mut Engine, sql: &str) {
    e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err:?}"));
}

#[test]
fn declare_percent_type_with_default_parses() {
    let mut e = Engine::new();
    ddl(&mut e, "CREATE TABLE t (id INT, name TEXT)");
    ddl(
        &mut e,
        "DO $$ DECLARE x t.id%TYPE := 42; \
         BEGIN ASSERT x = 42; END $$;",
    );
}

#[test]
fn declare_percent_type_text_column_with_default() {
    let mut e = Engine::new();
    ddl(&mut e, "CREATE TABLE u (label TEXT)");
    ddl(
        &mut e,
        "DO $$ DECLARE msg u.label%TYPE := 'hello'; \
         BEGIN ASSERT msg = 'hello'; END $$;",
    );
}

#[test]
fn declare_percent_rowtype_with_default() {
    // %ROWTYPE without a default is queued behind proper composite
    // type tracking (v7.40). With a default expression that IS a
    // scalar (not a row) the value drives the inferred type, so the
    // %ROWTYPE annotation is effectively a comment for the reader.
    let mut e = Engine::new();
    ddl(&mut e, "CREATE TABLE r (id INT)");
    ddl(
        &mut e,
        "DO $$ DECLARE row r%ROWTYPE := 1; \
         BEGIN ASSERT row = 1; END $$;",
    );
}
