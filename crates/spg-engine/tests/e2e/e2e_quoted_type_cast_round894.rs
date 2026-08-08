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
//! first two were fixed here; round 896 closed `regclass` and `regtype`
//! by folding their quoted `Named` spelling onto the `CastTarget` variant
//! that already had an arm. All four now cast under either spelling.

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

/// Round 896 closed the other two, and this is the test that made it a
/// deliberate act rather than a silent flip: it asserted they were still
/// refused, and it FAILED the moment the fold landed, with its own message
/// saying to rewrite it here.
///
/// They took a different route from `tsvector` / `tsquery`, which is why
/// round 894 left them rather than guessing. `regclass` and `regtype` have
/// no `DataType`; they are `CastTarget` variants with their own arm and
/// `Value::RegClass` / `Value::RegType` behind it. So the quoted `Named`
/// spelling is FOLDED onto the variant at the top of the cast evaluator —
/// one implementation, not a second arm that could drift — and the name is
/// added to the target validator, which runs first and would otherwise
/// reject it before the fold was reached.
#[test]
fn every_reg_type_casts_under_its_quoted_spelling_too() {
    let mut e = Engine::new();
    for ty in ["regclass", "regtype"] {
        assert!(
            scalar_ok(&mut e, &format!("SELECT NULL::{ty}")),
            "bare ::{ty} works"
        );
        assert!(
            scalar_ok(&mut e, &format!("SELECT NULL::\"{ty}\"")),
            "quoted ::\"{ty}\" should cast the same type"
        );
    }
    // And the fold reaches the arm, not just the validator: a real value
    // has to come back resolved, the same either way.
    assert_eq!(
        type_of(&mut e, "SELECT ('pg_class'::\"regclass\")::text"),
        type_of(&mut e, "SELECT ('pg_class'::regclass)::text"),
    );
    assert_eq!(
        type_of(&mut e, "SELECT ('int4'::\"regtype\")::text"),
        type_of(&mut e, "SELECT ('int4'::regtype)::text"),
    );
}
