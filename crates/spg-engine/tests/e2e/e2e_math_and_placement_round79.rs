//! v7.39 (read01 round 79) — sweeps of the aggregate FILTER/DISTINCT surface
//! (35 probes) and the numeric surface (36). The aggregate family came back
//! clean. The numeric one did not.
//!
//! 1. **exp() and ln() were wrong in the last digits.** `exp(1)` gave
//!    2.7182818284590446 where PG gives 2.718281828459045. Seven of nine probed
//!    exp() inputs were off, plus ln(10), plus cosh(1) (which is built on exp).
//!
//!    The code carried a note explaining why: "libm::exp was evaluated but is
//!    itself ~1 ULP off PG's exp on e.g. exp(1), so it is not a clean drop-in
//!    win — kept the existing series." Re-measured (a note left behind can be
//!    wrong — measure before acting on it): the note is right about the `libm`
//!    crate and stopped there, keeping a hand-rolled Taylor series that is far
//!    worse. The platform's libm — the very C library PG itself calls — matched
//!    PG on every probe. So calling it is not an approximation OF PG's answer;
//!    on the same host it IS PG's answer.
//!
//! 2. **A misplaced aggregate reported the wrong thing entirely.** `sum(sum(v))`
//!    and `WHERE sum(v) > 1` both reached the scalar function dispatcher, which
//!    said `unknown function sum`. The dispatcher cannot know better — it sees a
//!    call, not the clause it came from. The statement knows, so the check moved
//!    there. (Round 78 found exactly this shape with SRFs: the symptom two
//!    layers above the cause, wearing "no such function".)
//!
//! 3. **setseed returned NULL.** It returns `void`, and void is not null: PG
//!    renders it as the empty string, so `'x' || setseed(0.5)::text` is `x`. A
//!    NULL there swallowed every expression that wrapped it — and two existing
//!    tests, one of them named "pg_differential", had pinned the NULL.

use spg_engine::{Engine, QueryResult};

fn r1(e: &mut Engine, sql: &str) -> String {
    match e
        .execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"))
    {
        QueryResult::Rows { rows, .. } => spg_engine::eval::value_to_text(&rows[0].values[0]),
        other => panic!("{sql}: {other:?}"),
    }
}

#[test]
fn a_exp_and_ln_match_pg_to_the_last_digit() {
    let mut e = Engine::new();
    assert_eq!(r1(&mut e, "SELECT exp(1)"), "2.718281828459045");
    assert_eq!(r1(&mut e, "SELECT exp(2)"), "7.38905609893065");
    assert_eq!(r1(&mut e, "SELECT exp(-1)"), "0.36787944117144233");
    assert_eq!(r1(&mut e, "SELECT exp(5)"), "148.4131591025766");
    assert_eq!(r1(&mut e, "SELECT exp(-5)"), "0.006737946999085467");
    assert_eq!(r1(&mut e, "SELECT exp(10)"), "22026.465794806718");
    assert_eq!(r1(&mut e, "SELECT exp(709)"), "8.218407461554972e+307");
    assert_eq!(r1(&mut e, "SELECT ln(10)"), "2.302585092994046");
    assert_eq!(r1(&mut e, "SELECT ln(2)"), "0.6931471805599453");
    // cosh is exp-derived; the libm crate is a ULP off PG here too.
    assert_eq!(r1(&mut e, "SELECT cosh(1)"), "1.5430806348152437");
    assert_eq!(r1(&mut e, "SELECT sinh(1)"), "1.1752011936438014");
    // Untouched by the change, and still exact.
    assert_eq!(r1(&mut e, "SELECT sqrt(2)"), "1.4142135623730951");
    assert_eq!(r1(&mut e, "SELECT exp(0)"), "1");
}

#[test]
fn b_a_misplaced_aggregate_says_so() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE n (v int)").unwrap();
    e.execute("INSERT INTO n VALUES (1),(2)").unwrap();

    let nested = e.execute("SELECT sum(sum(v)) FROM n").unwrap_err();
    assert!(
        format!("{nested:?}").contains("cannot be nested"),
        "got {nested:?}"
    );
    let nested2 = e.execute("SELECT sum(v + max(v)) FROM n").unwrap_err();
    assert!(
        format!("{nested2:?}").contains("cannot be nested"),
        "got {nested2:?}"
    );
    let in_where = e
        .execute("SELECT count(*) FROM n WHERE sum(v) > 1")
        .unwrap_err();
    assert!(
        format!("{in_where:?}").contains("not allowed in WHERE"),
        "got {in_where:?}"
    );
    // HAVING is where an aggregate belongs, and still works.
    assert_eq!(
        r1(
            &mut e,
            "SELECT count(*) FROM n GROUP BY v HAVING sum(v) > 1"
        ),
        "1"
    );
}

#[test]
fn c_setseed_returns_void_not_null() {
    let mut e = Engine::new();
    assert_eq!(r1(&mut e, "SELECT setseed(0.5) IS NULL"), "false");
    // The point of it not being NULL: it does not swallow its surroundings.
    assert_eq!(r1(&mut e, "SELECT 'x' || setseed(0.5)::text"), "x");
}

#[test]
fn d_aggregate_filter_and_distinct_still_hold() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE a (g text, v int, t text)").unwrap();
    e.execute(
        "INSERT INTO a VALUES ('x',1,'p'),('x',2,'q'),('x',2,'p'),('y',3,'r'),('y',NULL,'s')",
    )
    .unwrap();
    assert_eq!(
        r1(
            &mut e,
            "SELECT count(DISTINCT v) FILTER (WHERE v > 1) FROM a"
        ),
        "2"
    );
    assert_eq!(
        r1(
            &mut e,
            "SELECT string_agg(DISTINCT t, ',' ORDER BY t) FILTER (WHERE t <> 'p') FROM a"
        ),
        "q,r,s"
    );
    assert_eq!(
        r1(&mut e, "SELECT array_agg(DISTINCT v) FROM a"),
        "{1,2,3,NULL}"
    );
    // An empty FILTER makes count 0 but sum NULL — PG's distinction.
    assert_eq!(
        r1(&mut e, "SELECT count(*) FILTER (WHERE v > 5) FROM a"),
        "0"
    );
    assert_eq!(
        r1(&mut e, "SELECT sum(v) FILTER (WHERE v > 5) IS NULL FROM a"),
        "true"
    );
}
