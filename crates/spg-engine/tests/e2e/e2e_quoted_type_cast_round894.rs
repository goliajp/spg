//! r894 — a cast to a QUOTED type name resolves the same type the bare
//! spelling does.
//!
//! `::tsvector` is a keyword arm in the cast parser and worked.
//! `::"tsvector"` becomes `CastTarget::Named("tsvector")` and is resolved
//! by name in the engine, where four names were missing — so PG18's own
//! spelling answered `type "tsvector" does not exist`. Quoted identifiers
//! are what ORMs and pg_dump output generate, so this is the path a
//! client reaches first, not an exotic one.
//!
//! Scope came from enumerating PG18's catalog rather than guessing: of
//! its 75 builtin scalar/range types, PG accepts every one quoted and SPG
//! rejected exactly `tsvector`, `tsquery`, `regclass`, `regtype`. The
//! first two are fixed here. The other two have no `DataType` of their
//! own — they live as `Value::RegClass` / `Value::RegType` with casts
//! special-cased at value level — so they are left failing rather than
//! guessed at, and the last test pins that they still behave as they did
//! so the gap cannot close by accident and go unnoticed.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn scalar_ok(e: &mut Engine, sql: &str) -> bool {
    matches!(e.execute(sql), Ok(QueryResult::Rows { .. }))
}

fn type_of(e: &mut Engine, sql: &str) -> String {
    match e
        .execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"))
    {
        QueryResult::Rows { rows, .. } => match &rows[0].values[0] {
            Value::Text(s) => s.to_string(),
            other => panic!("{sql}: {other:?}"),
        },
        other => panic!("{sql}: {other:?}"),
    }
}

#[test]
fn quoted_and_bare_type_names_cast_alike() {
    let mut e = Engine::new();
    for ty in ["tsvector", "tsquery"] {
        assert!(
            scalar_ok(&mut e, &format!("SELECT NULL::{ty}")),
            "bare ::{ty} should cast"
        );
        assert!(
            scalar_ok(&mut e, &format!("SELECT NULL::\"{ty}\"")),
            "quoted ::\"{ty}\" should cast the same type — this is what an \
             ORM or pg_dump writes"
        );
    }
}

#[test]
fn a_quoted_cast_carries_the_value_the_bare_one_does() {
    let mut e = Engine::new();
    assert_eq!(
        type_of(&mut e, "SELECT pg_typeof('a fat cat'::\"tsvector\")::text"),
        type_of(&mut e, "SELECT pg_typeof('a fat cat'::tsvector)::text"),
    );
}

/// The two still open, pinned so closing them is a deliberate act.
#[test]
fn regclass_and_regtype_are_still_quoted_only_by_their_bare_spelling() {
    let mut e = Engine::new();
    for ty in ["regclass", "regtype"] {
        assert!(
            scalar_ok(&mut e, &format!("SELECT NULL::{ty}")),
            "bare ::{ty} works"
        );
        assert!(
            !scalar_ok(&mut e, &format!("SELECT NULL::\"{ty}\"")),
            "::\"{ty}\" is still refused — when this starts passing, drop \
             this test and say so in the ledger rather than letting it flip \
             silently"
        );
    }
}
