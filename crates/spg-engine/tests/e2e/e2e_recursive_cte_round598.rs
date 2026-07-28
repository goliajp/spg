//! v7.39 (round 598) — a recursive CTE rebuilt the whole engine every round.
//!
//! The cost was linear in the recursion depth, which is right, but the
//! constant was 2.2 µs a round against PG18's 0.165. A counting allocator
//! named it without a profile: **63 allocations and 104 kB per iteration**,
//! or a gigabyte for a 10,000-row recursive CTE — and none of it varied with
//! how much else was in the catalog (100 / 10,000 / 200,000 rows in a
//! neighbouring table all gave byte-identical counts), so it was not the
//! catalog clone being deep. It was the loop body: every round cloned the
//! catalog, created the CTE table, inserted the working set, and constructed
//! a whole `Engine` — which initialises 82 fields — to hold it, then cloned
//! each recursive term to strip its CTE list.
//!
//! All of that is the same every round. The engine and its catalog are built
//! once, the terms are cloned once, and the CTE table is truncated and
//! refilled rather than dropped and recreated:
//!
//!     depth    before     after     PG18
//!      2500     15.63      4.09     0.63
//!      5000     16.24      6.74     0.85
//!     10000     29.73     11.93     1.44
//!     20000     44.44     24.29     2.68
//!     40000     87.24     47.24     5.37
//!     20000 x 3 cols  53.62  30.67  3.41
//!
//! 14.8x against PG at the sweep's size, 9.1x now. Allocations went 63 -> 39
//! a round; the bytes barely moved, because what is left is
//! `exec_select_cancel` — the cost of running a SELECT at all, which is the
//! general engine path and not recursion's to fix.
//!
//! What the pins are for. One engine now spans every round, so anything that
//! used to be reset by construction is not: the CTE table is refilled by
//! truncation, and `truncate` deliberately does not reset the row-id
//! counter. These check that each round still sees exactly its own working
//! set — through dedup, multiple anchors, several recursive terms, a
//! subquery over an outer table evaluated per round, and text that grows
//! with the recursion.
//!
//! Fourteen of the sixteen shapes matched live PG18 byte for byte. The other
//! two are about which queries are LEGAL, are unchanged by this round (the
//! same file run against the previous binary gives byte-identical SPG
//! output), and are recorded in the ledger: PG's message for a type mismatch
//! between the anchor and the recursive term is differently worded, and PG
//! REJECTS two recursive terms in one CTE where SPG answers.

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
    e.execute("CREATE TABLE tree (id INT, parent INT, label TEXT)").unwrap();
    e.execute(
        "INSERT INTO tree VALUES (1,NULL,'root'),(2,1,'a'),(3,1,'b'),(4,2,'a1'),(5,2,'a2'),\
         (6,3,'b1'),(7,6,'b1x')",
    )
    .unwrap();
    e
}

/// Each round must see exactly its own working set — the counter is the
/// shape that shows a leak immediately.
#[test]
fn round598_counters_and_dedup() {
    let mut e = seed();
    assert_eq!(
        vals(
            &mut e,
            "WITH RECURSIVE r(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM r WHERE n < 10) \
             SELECT sum(n), count(*) FROM r"
        ),
        vec!["55|10"]
    );
    assert_eq!(
        vals(
            &mut e,
            "WITH RECURSIVE r(n) AS (SELECT 1 UNION SELECT (n+1) % 5 FROM r WHERE n < 100) \
             SELECT count(*), sum(n) FROM r"
        ),
        vec!["5|10"],
        "UNION dedups, so the cycle terminates"
    );
    assert_eq!(
        vals(
            &mut e,
            "WITH RECURSIVE r(n) AS (SELECT 1 UNION ALL SELECT 100 UNION ALL \
             SELECT n+1 FROM r WHERE n < 5) SELECT n FROM r ORDER BY n"
        ),
        vec!["1", "2", "3", "4", "5", "100"],
        "a non-recursive UNION member is an extra anchor, not a term to re-run"
    );
    assert_eq!(
        vals(
            &mut e,
            "WITH RECURSIVE r(n) AS (SELECT 1 WHERE false UNION ALL SELECT n+1 FROM r WHERE n < 5) \
             SELECT count(*) FROM r"
        ),
        vec!["0"],
        "an empty anchor recurses nowhere"
    );
    assert_eq!(
        vals(
            &mut e,
            "WITH RECURSIVE r(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM r WHERE n < 0) \
             SELECT count(*), sum(n) FROM r"
        ),
        vec!["1|1"],
        "and a recursion that stops on the first round keeps its anchor"
    );
}

/// The shapes that read something other than the working set each round:
/// the base table, a subquery over it, and a join back into it.
#[test]
fn round598_rounds_see_the_outer_database() {
    let mut e = seed();
    assert_eq!(
        vals(
            &mut e,
            "WITH RECURSIVE t(id, parent, label, depth) AS \
             (SELECT id, parent, label, 0 FROM tree WHERE parent IS NULL \
              UNION ALL SELECT c.id, c.parent, c.label, t.depth+1 FROM tree c \
              JOIN t ON c.parent = t.id) SELECT id, label, depth FROM t ORDER BY id"
        ),
        vec![
            "1|root|0",
            "2|a|1",
            "3|b|1",
            "4|a1|2",
            "5|a2|2",
            "6|b1|2",
            "7|b1x|3",
        ]
    );
    assert_eq!(
        vals(
            &mut e,
            "WITH RECURSIVE r(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM r \
             WHERE n < (SELECT count(*) FROM tree)) SELECT count(*) FROM r"
        ),
        vec!["7"],
        "a subquery over the outer table, evaluated every round"
    );
}

/// Values that grow with the recursion, and NULLs carried through it.
#[test]
fn round598_growing_and_null_columns() {
    let mut e = seed();
    assert_eq!(
        vals(
            &mut e,
            "WITH RECURSIVE t(id, path) AS (SELECT id, label FROM tree WHERE parent IS NULL \
             UNION ALL SELECT c.id, t.path || '/' || c.label FROM tree c \
             JOIN t ON c.parent = t.id) SELECT id, path FROM t ORDER BY id"
        ),
        vec![
            "1|root",
            "2|root/a",
            "3|root/b",
            "4|root/a/a1",
            "5|root/a/a2",
            "6|root/b/b1",
            "7|root/b/b1/b1x",
        ]
    );
    assert_eq!(
        vals(
            &mut e,
            "WITH RECURSIVE r(n, s) AS (SELECT 1, 'x' UNION ALL SELECT n+1, s || 'y' FROM r \
             WHERE n < 6) SELECT n, s FROM r ORDER BY n"
        ),
        vec!["1|x", "2|xy", "3|xyy", "4|xyyy", "5|xyyyy", "6|xyyyyy"]
    );
    assert_eq!(
        vals(
            &mut e,
            "WITH RECURSIVE r(n, m) AS (SELECT 1, NULL::INT UNION ALL SELECT n+1, m FROM r \
             WHERE n < 4) SELECT n, m FROM r ORDER BY n"
        ),
        vec!["1|NULL", "2|NULL", "3|NULL", "4|NULL"]
    );
}

/// Two recursive terms in one CTE. PG REJECTS this — "recursive reference to
/// query r must not appear within its non-recursive term" — and SPG answers
/// it. That divergence is older than this round and is recorded in the
/// ledger; the assertion is here because two terms per round is exactly what
/// a shared engine could get wrong, and this is the only shape that exercises
/// it.
#[test]
fn round598_two_recursive_terms_share_a_round() {
    let mut e = seed();
    assert_eq!(
        vals(
            &mut e,
            "WITH RECURSIVE r(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM r WHERE n < 4 \
             UNION ALL SELECT n+10 FROM r WHERE n < 4) SELECT n FROM r ORDER BY n"
        ),
        vec!["1", "2", "3", "4", "11", "12", "13"]
    );
}

/// The CTE consumed more than once, wrapped in another CTE, and at a depth
/// where the per-round rebuild used to dominate.
#[test]
fn round598_consumption_and_scale() {
    let mut e = seed();
    assert_eq!(
        vals(
            &mut e,
            "WITH RECURSIVE r(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM r WHERE n < 5) \
             SELECT (SELECT count(*) FROM r) a, (SELECT sum(n) FROM r) b"
        ),
        vec!["5|15"]
    );
    assert_eq!(
        vals(
            &mut e,
            "WITH RECURSIVE r(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM r WHERE n < 6), \
             s AS (SELECT n*2 m FROM r) SELECT sum(m) FROM s"
        ),
        vec!["42"]
    );
    assert_eq!(
        vals(
            &mut e,
            "WITH RECURSIVE r(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM r WHERE n < 50) \
             SELECT n FROM r ORDER BY n DESC LIMIT 3"
        ),
        vec!["50", "49", "48"]
    );
    assert_eq!(
        vals(
            &mut e,
            "WITH RECURSIVE r(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM r WHERE n < 3000) \
             SELECT count(*), sum(n), min(n), max(n) FROM r"
        ),
        vec!["3000|4501500|1|3000"],
        "3000 rounds through one engine"
    );
}
