//! Round 694 — the array forms of the system types.
//!
//! F22 was recorded as "`'text'::regtype::oid` errors, PG answers 25", with a
//! root cause and a plan. Re-measured against PG18 this round: SPG answers
//! **25**, and 6 of 7 probes over the reg* family are byte-identical —
//! including `'upper'::regproc`'s `more than one function named "upper"`,
//! word for word. Round 667's `DataType::Oid` most likely carried it. The
//! ledger line had gone stale exactly as F33's had.
//!
//! What was actually missing was the ARRAY face:
//!
//!   * `regtype[]` / `regclass[]` — a SYNTAX error at the `]`. Those two
//!     scalars have dedicated `CastTarget` variants, so they never reached
//!     the postfix-`[]` handling every other type name goes through.
//!   * `oid[]` / `name[]` — parsed, then met `type "oid_array" does not
//!     exist`.
//!
//! `oid[]` got its own `DataType::OidArray` rather than being mapped onto
//! `BigIntArray`, and the reason is round 667's: the mapping answers
//! `pg_typeof('{1,2}'::oid[])` with `bigint[]`, which is the defect that
//! round closed for the scalar. The BODY is a BigIntArray's, byte for byte —
//! only the declared type differs, which is the whole point.
//!
//! One residual, measured and left: `ARRAY['text'::regtype]` gives `{25}`
//! where PG gives `{text}`. An edit to the array-element classifier did not
//! change it, which means that is not the path — so the edit came back out
//! rather than shipping as unproven wiring.

use spg_engine::{Engine, QueryResult};

fn one(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err}")) {
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
            .join(","),
        other => panic!("{sql}: {other:?}"),
    }
}

/// The scalar round trip F22 was filed for. PG18: 25, 23, text.
#[test]
fn round694_the_regtype_round_trip_f22_was_filed_for() {
    let mut e = Engine::new();
    assert_eq!(
        one(
            &mut e,
            "SELECT 'text'::regtype::oid, 'int4'::regtype::oid, 'text'::regtype::text"
        ),
        "25|23|text"
    );
    assert_eq!(one(&mut e, "SELECT 25::oid::regtype, 23::regtype"), "text|integer");
    assert_eq!(
        one(&mut e, "SELECT 'pg_class'::regclass::oid, 'pg_class'::regclass::text"),
        "1259|pg_class"
    );
}

/// The four array casts that did not exist. All PG18-verified.
#[test]
fn round694_the_system_array_casts_exist() {
    let mut e = Engine::new();
    // regtype[] canonicalises every element, as the scalar does: int4 →
    // integer. That is why it cannot simply keep the literal.
    assert_eq!(one(&mut e, "SELECT '{text,int4}'::regtype[]"), "{text,integer}");
    assert_eq!(one(&mut e, "SELECT '{pg_class}'::regclass[]"), "{pg_class}");
    assert_eq!(one(&mut e, "SELECT '{1,2}'::oid[]"), "{1,2}");
    assert_eq!(one(&mut e, "SELECT '{a}'::name[]"), "{a}");
}

/// An unknown element is refused with PG's sentence, rather than kept.
#[test]
fn round694_an_unknown_regtype_element_is_refused() {
    let mut e = Engine::new();
    let err = e
        .execute("SELECT '{nosuchtype}'::regtype[]")
        .expect_err("must not keep an unresolvable type name");
    assert!(format!("{err}").contains("nosuchtype"), "{err}");
}

/// `oid[]` keeps its declared type. Mapped onto BigIntArray this answered
/// `bigint[]` — the scalar's round-667 defect, one level up.
#[test]
fn round694_an_oid_array_reports_its_own_type() {
    let mut e = Engine::new();
    assert_eq!(one(&mut e, "SELECT pg_typeof('{1,2}'::oid[])"), "oid[]");
    // And the scalar it was modelled on.
    assert_eq!(one(&mut e, "SELECT pg_typeof(1::oid)"), "oid");
}

/// A recorded difference, not an agreement: PG gives `{text}` here.
/// Pinned so the day it changes, someone sees it.
#[test]
fn round694_array_of_regtype_still_renders_the_oid() {
    let mut e = Engine::new();
    assert_eq!(one(&mut e, "SELECT ARRAY['text'::regtype]"), "{25}");
}
