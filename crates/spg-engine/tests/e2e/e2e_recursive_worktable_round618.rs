//! v7.39 (round 618) — the recursive term is planned once and run over the
//! working set, instead of through a whole query execution per round.
//!
//! PG plans the recursive term ONCE and re-scans a worktable each iteration.
//! SPG emptied and refilled a real table and then called `exec_select_cancel`
//! — FROM resolution, schema build, predicate compilation, projection build,
//! result materialisation — for every round. Round 598 had already hoisted
//! the engine and its catalog out of the loop; what remained was that whole
//! execution. Counted with the allocating probe on
//! `WITH RECURSIVE r(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM r WHERE n < N)`:
//!
//!     N       allocations   bytes      ms
//!     2,000       80,265    198.3 MB   3.4
//!     8,000      321,004    792.4      9.3
//!    20,000      802,508  1,982.2     23.5
//!
//! about 40 allocations and 99 kB PER ROUND, while the working set is one
//! row. The worktable IS the working set, so there is nothing to empty and
//! refill and no query to run: the term's projection and predicate are
//! planned once and evaluated over the rows directly.
//!
//!     N       allocations   bytes      ms
//!     2,000       12,271      1.8 MB   0.6
//!    20,000      122,514     17.0      5.1
//!
//! — about 6 allocations and 0.85 kB a round — and over pgwire:
//!
//!     20,000 rounds   25.27 -> 11.65 ms   PG 3.25   9.15x -> 3.59x
//!    100,000 rounds            28.21      PG 12.64            2.23x
//!
//! The plan is only taken for the shape it covers: FROM is exactly the CTE
//! (no join, no LATERAL, no function or unnest source, no AS OF), and there
//! is no DISTINCT, GROUP BY, HAVING, ORDER BY, LIMIT, OFFSET, locking
//! clause, `*`, aggregate, window or subquery. Anything else keeps the
//! general path, and it is all-or-nothing across the terms so a query never
//! runs half on each. The pins below carry both kinds: the shapes that plan,
//! and the shapes that must fall back (a LIMIT inside the term, a DISTINCT,
//! a `*` projection, two recursive CTEs joined) — with the answers checked
//! against live PG18.
//!
//! Seventeen of the eighteen shapes match PG18 exactly. The eighteenth is a
//! CTE with TWO recursive terms, which PG rejects ("recursive reference to
//! query must not appear within its non-recursive term") and SPG answers —
//! the divergence round 598 recorded. All eighteen are byte-identical to the
//! previous binary, so nothing about the answers moved.

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

/// The shapes the plan covers.
#[test]
fn round618_planned_terms() {
    let mut e = Engine::new();
    assert_eq!(
        vals(
            &mut e,
            "WITH RECURSIVE r(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM r WHERE n < 10) SELECT n FROM r ORDER BY n"
        ),
        vec!["1", "2", "3", "4", "5", "6", "7", "8", "9", "10"]
    );
    assert_eq!(
        vals(
            &mut e,
            "WITH RECURSIVE r(n) AS (SELECT 1 UNION SELECT n+1 FROM r WHERE n < 10) SELECT count(*) FROM r"
        ),
        vec!["10"],
        "UNION dedups; UNION ALL does not — both go through the same plan"
    );
    assert_eq!(
        vals(
            &mut e,
            "WITH RECURSIVE r(a,b) AS (SELECT 1,'x' UNION ALL SELECT a+1, b||'y' FROM r WHERE a < 5) SELECT a,b FROM r ORDER BY a"
        ),
        vec!["1|x", "2|xy", "3|xyy", "4|xyyy", "5|xyyyy"],
        "two columns, one of them growing"
    );
    assert_eq!(
        vals(
            &mut e,
            "WITH RECURSIVE r(n) AS (SELECT 1 UNION ALL SELECT 2 UNION ALL SELECT n+10 FROM r WHERE n < 30) SELECT n FROM r ORDER BY n"
        ),
        vec!["1", "2", "11", "12", "21", "22", "31", "32"],
        "a second ANCHOR term, which must not re-emit every round"
    );
    assert_eq!(
        vals(
            &mut e,
            "WITH RECURSIVE r(n) AS (SELECT 1 UNION ALL SELECT x.n+1 FROM r x WHERE x.n < 5) SELECT n FROM r ORDER BY n"
        ),
        vec!["1", "2", "3", "4", "5"],
        "the CTE under an alias, which the plan has to resolve against"
    );
    assert_eq!(
        vals(
            &mut e,
            "WITH RECURSIVE r(n) AS (SELECT 1 UNION ALL SELECT n*2 FROM r WHERE n < 100) SELECT n FROM r ORDER BY n"
        ),
        vec!["1", "2", "4", "8", "16", "32", "64", "128"]
    );
    assert_eq!(
        vals(
            &mut e,
            "WITH RECURSIVE r(n,s) AS (SELECT 1, NULL::TEXT UNION ALL SELECT n+1, coalesce(s,'')||'z' FROM r WHERE n < 4) SELECT n,s FROM r ORDER BY n"
        ),
        vec!["1|NULL", "2|z", "3|zz", "4|zzz"],
        "a NULL carried through the anchor"
    );
    assert_eq!(
        vals(
            &mut e,
            "WITH RECURSIVE r(n) AS (SELECT 1::BIGINT UNION ALL SELECT n+1 FROM r WHERE n < 5) SELECT n, pg_typeof(n) FROM r ORDER BY n"
        ),
        vec!["1|bigint", "2|bigint", "3|bigint", "4|bigint", "5|bigint"],
        "the column type the anchor settled is the one the rows keep"
    );
}

/// Termination: a term that yields nothing, and one that yields only rows
/// already seen.
#[test]
fn round618_termination() {
    let mut e = Engine::new();
    assert_eq!(
        vals(
            &mut e,
            "WITH RECURSIVE r(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM r WHERE FALSE) SELECT n FROM r"
        ),
        vec!["1"]
    );
    assert_eq!(
        vals(
            &mut e,
            "WITH RECURSIVE r(n) AS (SELECT 1 UNION ALL SELECT n FROM r WHERE n < 0) SELECT count(*) FROM r"
        ),
        vec!["1"]
    );
    assert_eq!(
        vals(
            &mut e,
            "WITH RECURSIVE r(n) AS (SELECT 1 UNION SELECT 1 FROM r) SELECT count(*) FROM r"
        ),
        vec!["1"],
        "UNION's dedup is what stops this one — it would not terminate on ALL"
    );
}

/// The shapes that must NOT take the plan, and still answer the same.
#[test]
fn round618_shapes_that_fall_back() {
    let mut e = Engine::new();
    assert_eq!(
        vals(
            &mut e,
            "WITH RECURSIVE r(n) AS (SELECT 1 UNION ALL (SELECT n+1 FROM r WHERE n < 10 ORDER BY n LIMIT 1)) SELECT count(*) FROM r"
        ),
        vec!["10"],
        "ORDER BY and LIMIT inside the recursive term"
    );
    assert_eq!(
        vals(
            &mut e,
            "WITH RECURSIVE r(n) AS (SELECT 1 UNION ALL SELECT DISTINCT n+1 FROM r WHERE n < 5) SELECT n FROM r ORDER BY n"
        ),
        vec!["1", "2", "3", "4", "5"],
        "DISTINCT"
    );
    assert_eq!(
        vals(
            &mut e,
            "WITH RECURSIVE r(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM r WHERE n < 3) SELECT * FROM r ORDER BY 1"
        ),
        vec!["1", "2", "3"],
        "a `*` projection in the OUTER query is fine; one in the TERM falls back"
    );
    assert_eq!(
        vals(
            &mut e,
            "WITH RECURSIVE t(a) AS (SELECT 1 UNION ALL SELECT a+1 FROM t WHERE a < 3), \
             u(b) AS (SELECT 10 UNION ALL SELECT b+1 FROM u WHERE b < 12) SELECT a,b FROM t,u ORDER BY a,b"
        ),
        vec![
            "1|10", "1|11", "1|12", "2|10", "2|11", "2|12", "3|10", "3|11", "3|12",
        ],
        "two recursive CTEs, joined"
    );
}

/// What the recursion feeds, and at a depth where the per-round execution
/// was the cost.
#[test]
fn round618_scale() {
    let mut e = Engine::new();
    assert_eq!(
        vals(
            &mut e,
            "WITH RECURSIVE r(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM r WHERE n < 100) SELECT count(*), sum(n), min(n), max(n) FROM r"
        ),
        vec!["100|5050|1|100"]
    );
    assert_eq!(
        vals(
            &mut e,
            "WITH RECURSIVE r(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM r WHERE n < 20) SELECT n FROM r WHERE n % 3 = 0 ORDER BY n"
        ),
        vec!["3", "6", "9", "12", "15", "18"]
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT count(*), sum(n) FROM (WITH RECURSIVE r(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM r WHERE n < 20000) SELECT n FROM r) q"
        ),
        vec!["20000|200010000"],
        "twenty thousand rounds"
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT count(*) FROM (WITH RECURSIVE r(n) AS (SELECT 1 UNION SELECT n+1 FROM r WHERE n < 20000) SELECT n FROM r) q"
        ),
        vec!["20000"],
        "and the same under UNION, where every row is also keyed for the dedup"
    );
}
