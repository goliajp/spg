//! v7.39 (round 609) — COALESCE evaluated branches PG never evaluates, and
//! raised their errors.
//!
//! PG's COALESCE stops at the first non-NULL branch. This one evaluated all
//! of them into a `Vec` and then picked, so a later branch that fails took
//! the whole query down. Asked live, PG18 against SPG before this round:
//!
//!     coalesce(1, 1/0)          PG 1   SPG ERROR: division by zero
//!     coalesce(NULL, 2, 1/0)    PG 2   SPG ERROR: division by zero
//!     coalesce(1, NULL, 1/0)    PG 1   SPG ERROR: division by zero
//!
//! `coalesce(a, b/c)` is an ordinary defensive spelling, so this was a query
//! that works on PG and fails here. It now evaluates left to right and stops.
//!
//! What the branches after the pick are still needed for is their TYPE —
//! that is what the result's type comes from, and `COALESCE(1, 2.5)` is
//! numeric in both engines. So they are still read, but their error is
//! DISCARDED: PG never runs them and so never reports one, and a branch that
//! fails simply contributes no type. All twelve type-resolution shapes
//! measured before the change (int/numeric, int/bigint, int/float8,
//! smallint/int, a typed NULL sibling in either position, date, time, text)
//! answer exactly as they did, and as PG18 does.
//!
//! One shape moved, and it moved onto an already-recorded divergence rather
//! than off one: `coalesce('x', ('abc')::INT::TEXT)` used to fail here
//! because SPG evaluated the second branch, which is also what PG reports —
//! but PG reports it from CONSTANT FOLDING AT PLAN TIME, which SPG does not
//! do (round 605 recorded that same gap for `SELECT 1/0 FROM t WHERE false`).
//! With the short circuit, SPG answers `x`. It is the round-605 divergence,
//! not a new one; the ledger has it.
//!
//! The allocation this also removes is real but did not move the wall clock:
//! `nullif` went from 2 allocations a row to 1 over 200k rows, and COALESCE
//! stopped building two `Vec`s per row — yet over pgwire on 500k rows the
//! three shapes measured 99.25 -> 99.03, 43.55 -> 48.42 and 68.94 -> 66.16
//! ms, which is noise in both directions. Recorded as throughput-neutral;
//! `coalesce(id, 0)` still costs two allocations a row against one for
//! `abs(id)`, and greatest / least cost four, none of it located.

use spg_engine::{Engine, QueryResult};

fn vals(e: &mut Engine, sql: &str) -> Vec<String> {
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
            .collect(),
        other => panic!("{sql}: {other:?}"),
    }
}

fn seed() -> Engine {
    let mut e = Engine::new();
    e.execute("CREATE TABLE nt (id INT, g INT, s TEXT, n NUMERIC)").unwrap();
    e.execute(
        "INSERT INTO nt VALUES (1,10,'a',1.5),(2,NULL,NULL,NULL),(3,30,'c',3.0),\
         (NULL,40,'',4.25)",
    )
    .unwrap();
    e
}

/// The defect: a branch past the first non-NULL one must not run.
#[test]
fn round609_branches_after_the_pick_do_not_run() {
    let mut e = Engine::new();
    assert_eq!(
        vals(&mut e, "SELECT coalesce(1, 1/0), coalesce(NULL, 2, 1/0), coalesce(1, NULL, 1/0)"),
        vec!["1|2|1"],
        "PG answers 1, 2 and 1; this used to raise division by zero for all three"
    );
    assert_eq!(
        vals(&mut e, "SELECT coalesce(1, ('abc')::INT)"),
        vec!["1"],
        "and the same for a branch that fails to cast"
    );
    assert!(
        e.execute("SELECT coalesce(1/0, 1)").is_err(),
        "a branch BEFORE the pick is evaluated, so its error still fires"
    );
    assert!(
        e.execute("SELECT nullif(1/0, 5)").is_err(),
        "NULLIF is not short-circuit — PG evaluates both arms too"
    );
    assert_eq!(
        vals(&mut e, "SELECT coalesce(NULL::INT, NULL::INT), coalesce(NULL, NULL) IS NULL"),
        vec!["NULL|true"],
        "all-NULL is still NULL, and every branch ran to find that out"
    );
}

/// The type the result takes still comes from ALL the branches.
#[test]
fn round609_widening_is_unchanged() {
    let mut e = Engine::new();
    assert_eq!(
        vals(&mut e, "SELECT coalesce(1, 2.5), coalesce(1, 2.5)/2"),
        vec!["1|0.50000000000000000000"],
        "numeric, so the division is not an integer one"
    );
    assert_eq!(
        vals(&mut e, "SELECT coalesce(1, 2 + 0.5), coalesce(1, 2 + 0.5)/2"),
        vec!["1|0.50000000000000000000"],
        "the trailing branch is an EXPRESSION, and still contributes its type"
    );
    assert_eq!(
        vals(&mut e, "SELECT coalesce(1::INT, 2::BIGINT)"),
        vec!["1"]
    );
    assert_eq!(
        vals(&mut e, "SELECT coalesce(NULL::TIME,'12:00'), coalesce(NULL::DATE,'2020-01-02')"),
        vec!["12:00:00|2020-01-02"],
        "a typed NULL sibling still coerces the untyped literal"
    );
    assert_eq!(
        vals(&mut e, "SELECT nullif(1, 2.5), nullif(1,2.5)/2, nullif(2.5, 1)"),
        vec!["1|0.50000000000000000000|2.5"]
    );
    assert_eq!(
        vals(&mut e, "SELECT greatest(1, 2.5), least(1, 2.5), greatest(NULL, 3), least(NULL, 3)"),
        vec!["2.5|1|3|3"]
    );
    assert!(
        e.execute("SELECT coalesce(1, 'a'::TEXT)").is_err(),
        "the static branch-type check still refuses types that cannot be matched"
    );
    assert!(
        e.execute("SELECT nullif(1, 'a'::TEXT)").is_err(),
        "NULLIF inherits `=`'s operator resolution"
    );
}

/// Over rows, where the pick differs from row to row.
#[test]
fn round609_over_rows() {
    let mut e = seed();
    assert_eq!(
        vals(
            &mut e,
            "SELECT id, coalesce(g, -1), coalesce(s,'z'), coalesce(n, 0) FROM nt ORDER BY 1 NULLS LAST"
        ),
        vec!["1|10|a|1.5", "2|-1|z|0", "3|30|c|3.0", "NULL|40||4.25"]
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT id, coalesce(g, id, -1), coalesce(s, s, 'z'), coalesce(NULL, NULL, g) \
             FROM nt ORDER BY 1 NULLS LAST"
        ),
        vec!["1|10|a|10", "2|2|z|NULL", "3|30|c|30", "NULL|40||40"],
        "three branches, and the pick lands on a different one per row"
    );
    assert_eq!(
        vals(&mut e, "SELECT id, nullif(g, 10), nullif(s,'a'), nullif(n, 1.5) FROM nt ORDER BY 1 NULLS LAST"),
        vec!["1|NULL|NULL|NULL", "2|NULL|NULL|NULL", "3|30|c|3.0", "NULL|40||4.25"]
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT id, coalesce(nullif(s,'a'), 'z'), nullif(coalesce(s,'z'),'z') \
             FROM nt ORDER BY 1 NULLS LAST"
        ),
        vec!["1|z|a", "2|z|NULL", "3|c|c", "NULL||"],
        "nested, which is the shape the ledger had at 12.6x"
    );
    assert_eq!(
        vals(&mut e, "SELECT id FROM nt WHERE coalesce(g, 0) > 20 ORDER BY 1 NULLS LAST"),
        vec!["3", "NULL"],
        "in a predicate, where the compiled program runs it"
    );
    assert_eq!(
        vals(&mut e, "SELECT id FROM nt WHERE nullif(g, 30) IS NULL ORDER BY 1 NULLS LAST"),
        vec!["2", "3"]
    );
    assert_eq!(
        vals(&mut e, "SELECT count(coalesce(g, id)), count(nullif(g, 10)) FROM nt"),
        vec!["4|2"]
    );
    assert_eq!(
        vals(&mut e, "SELECT coalesce(s, 'x') || coalesce(s, 'y') FROM nt ORDER BY 1"),
        vec!["", "aa", "cc", "xy"]
    );
}

/// At a size where the branch that must not run would be found.
#[test]
fn round609_scale() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE big (id INT, g INT)").unwrap();
    e.execute("INSERT INTO big SELECT gg, CASE WHEN gg % 2 = 0 THEN NULL ELSE gg END \
               FROM generate_series(1, 20000) gg")
        .unwrap();
    assert_eq!(
        vals(&mut e, "SELECT count(*) FROM big WHERE coalesce(g, -1) = -1"),
        vec!["10000"],
        "half the rows fall through to the second branch"
    );
    assert_eq!(
        vals(&mut e, "SELECT sum(coalesce(g, 0)) FROM big"),
        vec!["100000000"]
    );
    assert_eq!(
        vals(&mut e, "SELECT count(*) FROM big WHERE coalesce(g, id / (id - id + 1)) > 0"),
        vec!["20000"],
        "the fallback divides, and the rows that never reach it never divide"
    );
    assert_eq!(
        vals(&mut e, "SELECT count(nullif(g, 1)) FROM big"),
        vec!["9999"]
    );
}
