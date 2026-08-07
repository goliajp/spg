//! v7.39 (round 255) — the ordered-set / hypothetical-set aggregate
//! surface (`percentile_cont/disc`, `mode`, and the WITHIN GROUP
//! `rank` family), swept 89 cases against live PG18.4 (2026-07-19).
//! The interpolation math, `mode`'s tie rule, the array-argument forms,
//! FILTER and the aggregate-internal ORDER BY all matched; the gaps:
//!
//!   * `percent_rank` / `cume_dist` divided by the NON-NULL row count.
//!     PG divides by the FULL input size — one extra NULL row moves
//!     `percent_rank(3)` from 2/6 to 2/7 (probed at 6, 7 and 9 rows).
//!     `rank` / `dense_rank` are unaffected either way, since they only
//!     count values sorting before the hypothetical row.
//!   * `percentile_cont` over a non-interpolatable ORDER BY type
//!     (text / date / timestamp / bool) answered NULL; PG declares the
//!     function only over the numeric tower and interval.
//!   * `percentile_cont(0.5, 0.6)` and `mode(1)` were accepted with the
//!     extra argument silently dropped.
//!   * The arity messages were SPG's own; PG resolves every one of
//!     these as a missing overload whose signature is `(direct args…,
//!     WITHIN GROUP args…)` — `function rank(integer, integer, text)
//!     does not exist`.
//!
//! Recorded residual: an untyped literal in a direct-argument position
//! reports `unknown` (PG's own placeholder) only when it is a bare
//! string or NULL; a numeric literal reports its resolved type, so
//! `rank(1,'a',2)` reads `rank(integer, unknown, integer, …)` where PG
//! also says `unknown` for the 'a'. The type check is static and
//! conservative (cast / column only — the round-237 rule), so an
//! expression whose type SPG cannot resolve statically is let through
//! rather than refused.

use spg_engine::{Engine, QueryResult};

fn seeded() -> Engine {
    let mut e = Engine::new();
    e.execute("CREATE TABLE os (g int, v int, n numeric, t text, d date, iv interval)")
        .unwrap();
    e.execute(
        "INSERT INTO os VALUES \
         (1,1,1.0,'a','2024-01-01','1 day'),(1,2,2.0,'b','2024-01-02','2 days'),\
         (1,3,3.0,'b','2024-01-03','3 days'),(1,4,4.0,'c','2024-01-04','4 days'),\
         (2,10,10.0,'x','2024-02-01','10 days'),(2,20,20.0,'y','2024-02-02','20 days'),\
         (2,NULL,NULL,NULL,NULL,NULL)",
    )
    .unwrap();
    e
}

fn one(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap_or_else(|x| panic!("{sql}: {x:?}")) {
        QueryResult::Rows { rows, .. } => spg_engine::eval::value_to_text(&rows[0].values[0]),
        other => panic!("{sql}: {other:?}"),
    }
}

fn err(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql) {
        Err(x) => format!("{x}"),
        Ok(ok) => panic!("{sql}: expected an error, got {ok:?}"),
    }
}

#[test]
fn hypothetical_fractions_divide_by_the_full_input_size() {
    // The three-point NULL matrix that pinned the rule: only the
    // denominator moves, and only for the two fractions.
    let mut e = Engine::new();
    e.execute("CREATE TABLE h (v int)").unwrap();
    e.execute("INSERT INTO h VALUES (1),(2),(3),(4),(10),(20)")
        .unwrap();
    let probe = |e: &mut Engine| {
        (
            one(e, "SELECT percent_rank(3) WITHIN GROUP (ORDER BY v) FROM h"),
            one(e, "SELECT cume_dist(3) WITHIN GROUP (ORDER BY v) FROM h"),
            one(e, "SELECT rank(3) WITHIN GROUP (ORDER BY v) FROM h"),
            one(e, "SELECT dense_rank(3) WITHIN GROUP (ORDER BY v) FROM h"),
        )
    };
    // 6 rows, no NULLs: (3-1)/6 and (3+1)/7.
    let (pr, cd, r, dr) = probe(&mut e);
    assert_eq!(
        (pr.as_str(), cd.as_str()),
        ("0.3333333333333333", "0.5714285714285714")
    );
    assert_eq!((r.as_str(), dr.as_str()), ("3", "3"));
    // 7 rows (one NULL): 2/7 and 4/8 — the NULL row counts.
    e.execute("INSERT INTO h VALUES (NULL)").unwrap();
    let (pr, cd, r, dr) = probe(&mut e);
    assert_eq!((pr.as_str(), cd.as_str()), ("0.2857142857142857", "0.5"));
    assert_eq!((r.as_str(), dr.as_str()), ("3", "3"));
    // 9 rows (three NULLs): 2/9 and 4/10.
    e.execute("INSERT INTO h VALUES (NULL),(NULL)").unwrap();
    let (pr, cd, r, dr) = probe(&mut e);
    assert_eq!((pr.as_str(), cd.as_str()), ("0.2222222222222222", "0.4"));
    assert_eq!((r.as_str(), dr.as_str()), ("3", "3"));
}

#[test]
fn percentile_cont_is_declared_only_over_numeric_and_interval() {
    let mut e = seeded();
    // Interpolatable: the numeric tower and interval.
    assert_eq!(
        one(
            &mut e,
            "SELECT percentile_cont(0.5) WITHIN GROUP (ORDER BY v) FROM os"
        ),
        "3.5"
    );
    assert_eq!(
        one(
            &mut e,
            "SELECT percentile_cont(0.5) WITHIN GROUP (ORDER BY n) FROM os"
        ),
        "3.5"
    );
    assert_eq!(
        one(
            &mut e,
            "SELECT pg_typeof(percentile_cont(0.5) WITHIN GROUP (ORDER BY iv)) FROM os"
        ),
        "interval"
    );
    // Refused — these answered NULL before.
    for (sql, want) in [
        (
            "SELECT percentile_cont(0.5) WITHIN GROUP (ORDER BY t) FROM os",
            "function percentile_cont(numeric, text) does not exist",
        ),
        (
            "SELECT percentile_cont(0.5) WITHIN GROUP (ORDER BY d) FROM os",
            "function percentile_cont(numeric, date) does not exist",
        ),
    ] {
        let got = err(&mut e, sql);
        assert!(got.contains(want), "{sql} → {got}");
    }
    // percentile_disc and mode take any sortable type (unchanged).
    assert_eq!(
        one(
            &mut e,
            "SELECT percentile_disc(0.5) WITHIN GROUP (ORDER BY t) FROM os"
        ),
        "b"
    );
    assert_eq!(
        one(&mut e, "SELECT mode() WITHIN GROUP (ORDER BY t) FROM os"),
        "b"
    );
    assert_eq!(
        one(
            &mut e,
            "SELECT percentile_disc(0.5) WITHIN GROUP (ORDER BY d) FROM os"
        ),
        "2024-01-03"
    );
}

#[test]
fn arity_mismatches_read_as_missing_overloads() {
    let mut e = seeded();
    for (sql, want) in [
        // One direct argument per sort key, or none at all.
        (
            "SELECT rank(3) WITHIN GROUP (ORDER BY v, t) FROM os",
            "function rank(integer, integer, text) does not exist",
        ),
        // Extra direct arguments were silently dropped before.
        (
            "SELECT percentile_cont(0.5, 0.6) WITHIN GROUP (ORDER BY v) FROM os",
            "function percentile_cont(numeric, numeric, integer) does not exist",
        ),
        (
            "SELECT mode(1) WITHIN GROUP (ORDER BY v) FROM os",
            "function mode(integer, integer) does not exist",
        ),
        // A multi-key sort spec belongs only to the hypothetical family.
        (
            "SELECT percentile_cont(0.5) WITHIN GROUP (ORDER BY v, t) FROM os",
            "function percentile_cont(numeric, integer, text) does not exist",
        ),
    ] {
        let got = err(&mut e, sql);
        assert!(got.contains(want), "{sql} → {got}");
    }
    // The well-formed multi-key hypothetical call still works.
    assert_eq!(
        one(
            &mut e,
            "SELECT rank(2,'b') WITHIN GROUP (ORDER BY v, t) FROM os"
        ),
        "2"
    );
}

#[test]
fn the_ordered_set_core_is_unchanged() {
    let mut e = seeded();
    for (sql, want) in [
        (
            "SELECT percentile_cont(0.0) WITHIN GROUP (ORDER BY v) FROM os",
            "1",
        ),
        (
            "SELECT percentile_cont(1.0) WITHIN GROUP (ORDER BY v) FROM os",
            "20",
        ),
        (
            "SELECT percentile_cont(0.25) WITHIN GROUP (ORDER BY v) FROM os",
            "2.25",
        ),
        (
            "SELECT percentile_disc(0.5) WITHIN GROUP (ORDER BY v) FROM os",
            "3",
        ),
        (
            "SELECT percentile_disc(0.25) WITHIN GROUP (ORDER BY v) FROM os",
            "2",
        ),
        ("SELECT mode() WITHIN GROUP (ORDER BY v) FROM os", "1"),
        // Array-argument forms.
        (
            "SELECT percentile_cont(ARRAY[0.25,0.5,0.75]) WITHIN GROUP (ORDER BY v) FROM os",
            "{2.25,3.5,8.5}",
        ),
        (
            "SELECT percentile_disc(ARRAY[0.0,1.0]) WITHIN GROUP (ORDER BY v) FROM os",
            "{1,20}",
        ),
        // Result types.
        (
            "SELECT pg_typeof(percentile_cont(0.5) WITHIN GROUP (ORDER BY v)) FROM os",
            "double precision",
        ),
        (
            "SELECT pg_typeof(percentile_disc(0.5) WITHIN GROUP (ORDER BY v)) FROM os",
            "integer",
        ),
        (
            "SELECT pg_typeof(rank(3) WITHIN GROUP (ORDER BY v)) FROM os",
            "bigint",
        ),
        // FILTER and the aggregate-internal ORDER BY.
        ("SELECT count(*) FILTER (WHERE v > 2) FROM os", "4"),
        (
            "SELECT percentile_cont(0.5) WITHIN GROUP (ORDER BY v) FILTER (WHERE v > 1) FROM os",
            "4",
        ),
        (
            "SELECT string_agg(t, ',' ORDER BY v DESC) FROM os",
            "y,x,c,b,b,a",
        ),
    ] {
        assert_eq!(one(&mut e, sql), want, "{sql}");
    }
    // An out-of-range fraction errors even before the rows are read.
    let got = err(
        &mut e,
        "SELECT percentile_cont(1.5) WITHIN GROUP (ORDER BY v) FROM os",
    );
    assert!(
        got.contains("percentile value 1.5 is not between 0 and 1"),
        "{got}"
    );
    // A NULL fraction, and an all-NULL input, are NULL.
    assert_eq!(
        one(
            &mut e,
            "SELECT percentile_cont(NULL) WITHIN GROUP (ORDER BY v) FROM os"
        ),
        "NULL"
    );
    assert_eq!(
        one(
            &mut e,
            "SELECT percentile_cont(0.5) WITHIN GROUP (ORDER BY v) FROM os WHERE v IS NULL"
        ),
        "NULL"
    );
}
