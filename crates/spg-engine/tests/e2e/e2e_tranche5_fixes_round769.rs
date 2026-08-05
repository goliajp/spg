//! Round 769 (F31 tranche 5, claims 121-150) — three fixes, all
//! PG18-measured:
//!
//! - #135: `#>` / `#>>` accept a real TEXT[] path value
//!   (`doc #> ARRAY['a','b']`), not only the `'{a,b}'` literal.
//! - #140: `CREATE TYPE x AS ()` — an attribute-less composite is
//!   legal PG; both the parser and a twin engine-side guard refused
//!   (and the old e2e note claimed PG requires an attribute).
//!   The empty record VALUE literal (`'()'::x`) is a ledgered
//!   residual.
//! - #150: `SET x TO DEFAULT` — DEFAULT lexes as its keyword token
//!   and the ident arm never saw it, so the everyday reset form was
//!   a syntax error.

use spg_engine::{Engine, QueryResult};

fn one(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => rows
            .iter()
            .map(|r| {
                r.values
                    .iter()
                    .map(spg_engine::eval::value_to_text)
                    .collect::<Vec<_>>()
                    .join("|")
            })
            .collect::<Vec<_>>()
            .join(";"),
        other => panic!("{other:?}"),
    }
}

#[test]
fn round769_json_path_accepts_text_array() {
    let mut e = Engine::new();
    assert_eq!(
        one(
            &mut e,
            "SELECT '{\"a\":{\"b\":1}}'::jsonb #> ARRAY['a','b'], \
             '{\"a\":{\"b\":1}}'::jsonb #>> ARRAY['a','b'], \
             '{\"a\":{\"b\":1}}'::jsonb #> '{a,b}'"
        ),
        "1|1|1"
    );
    // A NULL path element yields NULL, as PG does.
    assert_eq!(
        one(
            &mut e,
            "SELECT ('{\"a\":1}'::jsonb #> ARRAY['a', NULL]) IS NULL"
        ),
        "true"
    );
}

#[test]
fn round769_empty_composite_and_set_to_default() {
    let mut e = Engine::new();
    e.execute("CREATE TYPE t5empty AS ()").unwrap();
    e.execute("DROP TYPE t5empty").unwrap();
    e.execute("SET statement_timeout = 5000").unwrap();
    e.execute("SET statement_timeout TO DEFAULT").unwrap();
    assert_eq!(one(&mut e, "SHOW statement_timeout"), "0");
}
