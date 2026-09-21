//! 9.0.0 (N16) — a keyword could not be a PL/pgSQL name.
//!
//! PL/pgSQL's scanner is its own: PostgreSQL 18.6 accepts a block label
//! `<<inner>>` and a `DECLARE inner int`, because the body is scanned
//! by plpgsql and not by the SQL grammar. SPG refused both with
//! `expected identifier, got Inner`, and the refusal took the whole DO
//! block with it.
//!
//! Measured 2026-09-21 against PG 18.6:
//!
//! ```text
//! DO $$ <<inner>> DECLARE x int := 1; BEGIN RAISE NOTICE '%', x; END $$;
//!   PG: NOTICE  label ok 1      SPG: ERROR syntax error at or near "$$"
//! DO $$ DECLARE inner int := 2; BEGIN RAISE NOTICE '%', inner; END $$;
//!   PG: ERROR  syntax error at end of input (the REFERENCE, not the
//!              declaration — PG's SQL scanner reads the expression)
//!   SPG: ERROR syntax error at or near "$$"  (the DECLARATION)
//! ```
//!
//! So the declaration half is accepted now and the reference half stays
//! a syntax error, which is what PostgreSQL does with each.

use spg_engine::Engine;

#[test]
fn n16_a_reserved_keyword_is_a_legal_block_label() {
    let mut e = Engine::new();
    e.execute("DO $$ <<inner>> DECLARE x int := 1; BEGIN RAISE NOTICE '%', x; END $$")
        .expect("PG 18.6 accepts the label");
    // …and the label is usable, which is what a label is for.
    e.execute("DO $$ <<outer_block>> BEGIN LOOP EXIT outer_block; END LOOP; END $$")
        .expect("an EXIT naming its block");
}

#[test]
fn n16_a_reserved_keyword_is_a_legal_declared_name() {
    let mut e = Engine::new();
    e.execute("DO $$ DECLARE inner int := 2; BEGIN NULL; END $$")
        .expect("PG 18.6 accepts the declaration");
    e.execute("DO $$ DECLARE \"select\" int := 3; BEGIN NULL; END $$")
        .expect("quoted, which always worked");
    // The keyword spellings come from the lexer's own table, so a name
    // that is not a keyword at all is unaffected.
    e.execute("DO $$ DECLARE ordinary int := 4; BEGIN NULL; END $$")
        .expect("an ordinary name");
}
