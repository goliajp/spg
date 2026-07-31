//! v7.39 (round 649) — C08: the numeric ladder had a rung missing and a
//! branch that did not count.
//!
//! PG resolves `COALESCE`'s result from the DECLARED types of its
//! branches. SPG collected them from the evaluated VALUES, and two
//! things fell out of that:
//!
//!   * **A NULL branch contributed nothing.** `Value::Null` has no
//!     `data_type()`, so `coalesce(1::int, NULL::float8)` saw only
//!     `integer` and answered integer where PG answers double
//!     precision. The branch's type is a property of the expression,
//!     not of what it evaluated to.
//!   * **`real` was not on the ladder.** `numeric_rank` went 1, 2, 3, 4,
//!     … 6, with rank 5 plainly left for `real` and never filled — so a
//!     sibling set containing one failed the "all numeric" test and fell
//!     through untouched. `widen_value_to` had the same hole in its own
//!     list, which is why filling only the first one still left
//!     `coalesce(1::int, 1::real)` answering integer: two lists, one
//!     ladder, and the gap had to be closed in both.
//!
//! Everything else in the probe already matched, including the error
//! text for a set PG cannot resolve at all.

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

#[test]
fn round649_a_null_branch_still_carries_its_type() {
    let mut e = Engine::new();
    assert_eq!(
        one(&mut e, "SELECT pg_typeof(coalesce(1::int, NULL::float8))"),
        "double precision"
    );
    // …and the VALUE is still the first non-NULL one, widened.
    assert_eq!(one(&mut e, "SELECT coalesce(1::int, NULL::float8)"), "1");
    // The reverse order already worked, because the pick fell through.
    assert_eq!(
        one(&mut e, "SELECT pg_typeof(coalesce(NULL::int, 1.5::float8))"),
        "double precision"
    );
    // A NULL branch of a NARROWER type does not drag the result down.
    assert_eq!(
        one(&mut e, "SELECT pg_typeof(coalesce(1.5::float8, NULL::int))"),
        "double precision"
    );
}

#[test]
fn round649_real_is_on_the_ladder() {
    let mut e = Engine::new();
    assert_eq!(
        one(&mut e, "SELECT pg_typeof(coalesce(1::real, 1.5::float8))"),
        "double precision"
    );
    assert_eq!(
        one(&mut e, "SELECT pg_typeof(coalesce(1::int, 1::real))"),
        "real"
    );
    assert_eq!(
        one(&mut e, "SELECT pg_typeof(coalesce(1::smallint, 1::real))"),
        "real"
    );
}

/// The rungs that already worked, kept so filling the gap cannot
/// disturb them.
#[test]
fn round649_the_rest_of_the_ladder_is_unchanged() {
    let mut e = Engine::new();
    for (sql, want) in [
        ("coalesce(1::int, 1::bigint)", "bigint"),
        ("coalesce(1::smallint, 1::int)", "integer"),
        ("coalesce(1::int, 1.5::numeric)", "numeric"),
        ("coalesce(1::numeric, 1.5::float8)", "double precision"),
        ("coalesce(NULL::int, NULL::int)", "integer"),
        ("greatest(1::int, 1.5::float8)", "double precision"),
        ("least(1::int, 1.5::numeric)", "numeric"),
        ("CASE WHEN true THEN 1::int ELSE 1.5::float8 END", "double precision"),
    ] {
        assert_eq!(
            one(&mut e, &format!("SELECT pg_typeof({sql})")),
            want,
            "{sql}"
        );
    }
}

/// A set PG cannot resolve is still refused, in PG's words.
#[test]
fn round649_an_unresolvable_set_is_still_refused() {
    let mut e = Engine::new();
    let err = e
        .execute("SELECT coalesce(1::int, 'a'::text)")
        .expect_err("PG cannot match integer and text");
    assert!(
        err.to_string()
            .contains("COALESCE types integer and text cannot be matched"),
        "unexpected message: {err}"
    );
}

/// COALESCE still stops at the first non-NULL branch — the typing pass
/// reads the later ones for their type without running them for effect.
#[test]
fn round649_short_circuit_survives_the_typing_pass() {
    let mut e = Engine::new();
    assert_eq!(one(&mut e, "SELECT coalesce(1, 1/0)"), "1");
    assert_eq!(one(&mut e, "SELECT coalesce(NULL, 2, 1/0)"), "2");
    assert_eq!(one(&mut e, "SELECT coalesce(1, NULL, 1/0)"), "1");
}
