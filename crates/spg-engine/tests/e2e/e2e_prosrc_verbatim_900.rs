//! 9.0.0 — `pg_proc.prosrc` for a PL/pgSQL function was a re-render of
//! the parsed block, not the text that was written.
//!
//! Measured against PostgreSQL 18.6, which stores the text between the
//! dollar quotes verbatim. A body written as
//!
//! ```text
//!   \nBEGIN\n  -- a comment\n  RETURN x + 1;\nEND\n
//! ```
//!
//! came back as `BEGIN\n  RETURN (x + 1);\nEND`: the comment gone, the
//! parentheses added, the surrounding newlines gone. A `LANGUAGE sql`
//! body was already stored verbatim, so the two languages disagreed
//! about what `prosrc` is — and `pg_get_functiondef` inherited it, so a
//! dump did not carry the function's own source.
//!
//! The block is still parsed and still what the executor walks; the
//! source now rides beside it.

use spg_engine::{Engine, QueryResult};

fn col(e: &mut Engine, sql: &str) -> Vec<String> {
    let QueryResult::Rows { rows, .. } = e
        .execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"))
    else {
        panic!("expected rows for {sql}");
    };
    rows.into_iter()
        .map(|r| spg_engine::eval::value_to_text(r.values.first().expect("one column")))
        .collect()
}

const BODY: &str = "\nBEGIN\n  -- a comment\n  RETURN x + 1;\nEND\n";

#[test]
fn prosrc_is_the_text_that_was_written() {
    let mut e = Engine::new();
    e.execute(&format!(
        "CREATE FUNCTION d1(x int) RETURNS int AS $${BODY}$$ LANGUAGE plpgsql"
    ))
    .unwrap();
    assert_eq!(
        col(&mut e, "SELECT prosrc FROM pg_proc WHERE proname='d1'"),
        vec![BODY.to_string()]
    );
    // And the block behind it still runs.
    assert_eq!(col(&mut e, "SELECT d1(5)"), vec!["6".to_string()]);
}

#[test]
fn the_deparse_carries_the_same_text() {
    let mut e = Engine::new();
    e.execute(&format!(
        "CREATE FUNCTION d1(x int) RETURNS int AS $${BODY}$$ LANGUAGE plpgsql"
    ))
    .unwrap();
    let def = col(&mut e, "SELECT pg_get_functiondef('d1'::regproc)").remove(0);
    assert!(def.contains("-- a comment"), "{def}");
    assert!(def.contains("RETURN x + 1;"), "{def}");
    assert!(!def.contains("(x + 1)"), "the deparse re-rendered: {def}");
}

#[test]
fn a_sql_body_was_already_verbatim_and_stays_so() {
    let mut e = Engine::new();
    let sql_body = "\n  -- sql comment\n  SELECT x + 1;\n";
    e.execute(&format!(
        "CREATE FUNCTION d1s(x int) RETURNS int AS $${sql_body}$$ LANGUAGE sql"
    ))
    .unwrap();
    assert_eq!(
        col(&mut e, "SELECT prosrc FROM pg_proc WHERE proname='d1s'"),
        vec![sql_body.to_string()]
    );
}
