//! v7.39 (round 588) — the ANSI-89 join wrote its condition where nobody
//! read it.
//!
//! `FROM a, b WHERE a.id = b.id` is the older spelling of
//! `FROM a JOIN b ON a.id = b.id` and means the same join. SPG parsed the
//! comma as a `Cross` peer with no ON clause, and two separate pieces of
//! planning then failed on it: `analyze_join_pushdown` pushes single-relation
//! WHERE conjuncts only onto `Inner` peers, so `b.id < 100` never reached
//! `b`'s scan; and `extract_join_keys` only ever read the ON clause, so the
//! equality never became a join key. With no key the peer fell to the
//! nested-loop stage, which crosses the WHOLE peer against every surviving
//! left row.
//!
//! Warm sessions over pgwire, 500k rows a side, against PG18 on the same
//! client through the same pipe:
//!
//!     FROM j a, j b WHERE a.id = b.id AND …
//!       a.id < 10   AND b.id < 10       458.2 ->  19 ms    PG 10.2
//!       a.id < 100  AND b.id < 100     4599.9 ->  19.18    PG 10.3
//!       a.id < 1000 AND b.id < 1000   >20000  ->  19.42    PG  9.4
//!       a.id < 100 (one side only)     4515.6 ->  19.19
//!     CROSS JOIN j b WHERE a.id = b.id …4541.8 ->  19.38
//!     JOIN j b ON true WHERE a.id = …   5360.1 ->  19.37
//!
//! The `< 1000` row was a 20-second timeout, so that one is better than
//! 1000x. Every shape now lands on the ON form's own number (18.9 ms), which
//! is where it belongs: what is left is the ordinary 1.9x join loss that the
//! ON form has too, not a category difference.
//!
//! The cost was linear in (left survivors x peer rows) at about 90 ns a pair
//! — 10 x 500k = 5M pairs in 458 ms, 100 x 500k = 50M in 4600 — which is what
//! a full inner scan per outer row looks like.
//!
//! What the pins are for. The rewrite is sound only while every relation in
//! the chain is non-nullable: under an outer join a WHERE equality filters
//! AFTER the NULL-filling and is NOT the same thing as a join condition, so
//! promoting it there would change the answer. One outer join anywhere in the
//! chain gives up on the whole statement, and the LEFT / FULL pins below are
//! what says so. All 20 shapes in this file were checked against live PG18
//! and matched.

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

fn one(e: &mut Engine, sql: &str) -> String {
    vals(e, sql).first().cloned().unwrap_or_default()
}

fn seed() -> Engine {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t1 (id INT, k INT, s TEXT)")
        .unwrap();
    e.execute("CREATE TABLE t2 (id INT, k INT, s TEXT)")
        .unwrap();
    e.execute("CREATE TABLE t3 (id INT, k INT)").unwrap();
    e.execute("INSERT INTO t1 VALUES (1,10,'a'),(2,20,'b'),(3,NULL,'c'),(4,10,'d'),(5,30,NULL)")
        .unwrap();
    e.execute("INSERT INTO t2 VALUES (1,10,'a'),(2,99,'x'),(3,NULL,'y'),(6,10,'z')")
        .unwrap();
    e.execute("INSERT INTO t3 VALUES (1,10),(2,20),(7,70)")
        .unwrap();
    e
}

/// The shape the round is about, in its three spellings — comma,
/// `CROSS JOIN` and `JOIN … ON true` — all of which now take a join key
/// out of the WHERE clause.
#[test]
fn round588_ansi89_equality_joins() {
    let mut e = seed();
    for from in [
        "t1 a, t2 b",
        "t1 a CROSS JOIN t2 b",
        "t1 a JOIN t2 b ON true",
    ] {
        assert_eq!(
            vals(
                &mut e,
                &format!("SELECT a.id, b.s FROM {from} WHERE a.id = b.id ORDER BY 1")
            ),
            vec!["1|a", "2|x", "3|y"],
            "{from}"
        );
        assert_eq!(
            vals(
                &mut e,
                &format!("SELECT a.id, b.id FROM {from} WHERE a.id = b.id AND b.id < 3 ORDER BY 1")
            ),
            vec!["1|1", "2|2"],
            "{from} with a side predicate"
        );
        // Written the other way round, and with the table names unaliased.
        assert_eq!(
            vals(
                &mut e,
                &format!(
                    "SELECT a.id FROM {from} WHERE b.id = a.id AND a.s IS NOT NULL ORDER BY 1"
                )
            ),
            vec!["1", "2", "3"],
            "{from} reversed"
        );
    }
    assert_eq!(
        vals(
            &mut e,
            "SELECT t1.id FROM t1, t2 WHERE t1.id = t2.id ORDER BY 1"
        ),
        vec!["1", "2", "3"]
    );
}

/// A NULL key joins nothing, on either side — the same as an ON-clause
/// equality, and the reason promotion is safe at all.
#[test]
fn round588_null_keys_match_nothing() {
    let mut e = seed();
    // t1.k: 10, 20, NULL, 10, 30 — t2.k: 10, 99, NULL, 10.
    // The two NULLs must not meet; 10 pairs 2 x 2.
    assert_eq!(
        vals(
            &mut e,
            "SELECT a.id, b.id FROM t1 a, t2 b WHERE a.k = b.k ORDER BY 1, 2"
        ),
        vec!["1|1", "1|6", "4|1", "4|6"]
    );
    assert_eq!(
        one(&mut e, "SELECT count(*) FROM t1 a, t2 b WHERE a.s = b.s"),
        "1",
        "only 'a' = 'a'; the NULL text joins nothing"
    );
}

/// The trap. Under an outer join a WHERE equality is applied AFTER the
/// NULL-filling, so it must NOT become the join condition — promoting it
/// would resurrect rows the WHERE is there to remove.
#[test]
fn round588_outer_joins_never_promote() {
    let mut e = seed();
    // a.k = b.k matches (1,1), (1,6), (4,1), (4,6); the WHERE then keeps
    // only the pairs whose ids are also equal.
    assert_eq!(
        vals(
            &mut e,
            "SELECT a.id, b.id FROM t1 a LEFT JOIN t2 b ON a.k = b.k WHERE a.id = b.id ORDER BY 1"
        ),
        vec!["1|1"]
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT a.id, b.id FROM t1 a FULL OUTER JOIN t2 b ON a.k = b.k \
             WHERE a.id = b.id ORDER BY 1"
        ),
        vec!["1|1"]
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT a.id, b.id FROM t1 a RIGHT JOIN t2 b ON a.k = b.k WHERE a.id = b.id ORDER BY 1"
        ),
        vec!["1|1"]
    );
    // An outer join ANYWHERE in the chain stops promotion for every peer,
    // including the plain comma one that follows it.
    assert_eq!(
        vals(
            &mut e,
            "SELECT a.id, c.id FROM t1 a LEFT JOIN t2 b ON a.id = b.id, t3 c \
             WHERE a.id = c.id ORDER BY 1"
        ),
        vec!["1|1", "2|2"]
    );
    // And the LEFT join still keeps its unmatched rows when nothing filters
    // them.
    assert_eq!(
        vals(
            &mut e,
            "SELECT a.id, b.id FROM t1 a LEFT JOIN t2 b ON a.id = b.id ORDER BY 1"
        ),
        vec!["1|1", "2|2", "3|3", "4|NULL", "5|NULL"]
    );
}

/// Only a top-level `col = col` conjunct is a candidate. Anything under an
/// OR, or comparing against a constant, or any other operator stays exactly
/// where it was — and still filters.
#[test]
fn round588_only_top_level_column_equalities_promote() {
    let mut e = seed();
    assert_eq!(
        vals(
            &mut e,
            "SELECT a.id, b.id FROM t1 a, t2 b WHERE a.id = b.id OR a.k = b.k ORDER BY 1, 2"
        ),
        vec!["1|1", "1|6", "2|2", "3|3", "4|1", "4|6"],
        "an OR is one conjunct and is not an equi-key"
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT a.id, b.id FROM t1 a, t2 b WHERE a.id < b.id ORDER BY 1, 2"
        )
        .len(),
        8,
        "a non-equality is still a nested-loop filter"
    );
    assert_eq!(
        one(&mut e, "SELECT count(*) FROM t1 a, t2 b"),
        "20",
        "a bare cross product is still a cross product"
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT a.id FROM t1 a, t2 b WHERE a.id = b.id AND a.k = 10 ORDER BY 1"
        ),
        vec!["1"],
        "a constant comparison beside the key"
    );
    // A subquery conjunct next to the promoted equality is untouched.
    assert_eq!(
        vals(
            &mut e,
            "SELECT a.id FROM t1 a, t2 b WHERE a.id = b.id AND a.id IN (SELECT id FROM t3) \
             ORDER BY 1"
        ),
        vec!["1", "2"]
    );
}

/// Chains of three: the equality may name the primary or an earlier peer,
/// and every conjunct has to end up enforced exactly once.
#[test]
fn round588_multi_way_chains() {
    let mut e = seed();
    assert_eq!(
        vals(
            &mut e,
            "SELECT a.id, c.k FROM t1 a, t2 b, t3 c WHERE a.id = b.id AND b.id = c.id ORDER BY 1"
        ),
        vec!["1|10", "2|20"],
        "each peer keys off the one before it"
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT a.id, c.k FROM t1 a, t2 b, t3 c WHERE a.id = b.id AND a.id = c.id ORDER BY 1"
        ),
        vec!["1|10", "2|20"],
        "both peers key off the primary"
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT a.id, c.id FROM t1 a JOIN t2 b ON a.id = b.id, t3 c \
             WHERE b.id = c.id ORDER BY 1"
        ),
        vec!["1|1", "2|2"],
        "an ON-form join and a comma join in one statement"
    );
    // Two equalities on the same pair of relations: both are keys.
    assert_eq!(
        vals(
            &mut e,
            "SELECT a.id FROM t1 a, t2 b WHERE a.id = b.id AND a.k = b.k ORDER BY 1"
        ),
        vec!["1"]
    );
    // A self comma-join.
    assert_eq!(
        vals(
            &mut e,
            "SELECT a.id, b.id FROM t1 a, t1 b WHERE a.id = b.id AND a.id > 3 ORDER BY 1"
        ),
        vec!["4|4", "5|5"]
    );
}

/// EXPLAIN has to say what the executor does. Before this round the comma
/// form really was a nested loop and the plan was right about it; promoting
/// the key without telling EXPLAIN would have made it wrong, which is the
/// r551 defect over again. The two spellings are one query and now print one
/// plan — with the promoted conjunct named as the join's condition and no
/// longer repeated on the scan's Filter line.
#[test]
fn round588_explain_follows_the_executor() {
    let mut e = seed();
    let comma = vals(
        &mut e,
        "EXPLAIN SELECT count(*) FROM t1 a, t2 b WHERE a.id = b.id AND a.k = 10",
    );
    let on_form = vals(
        &mut e,
        "EXPLAIN SELECT count(*) FROM t1 a JOIN t2 b ON a.id = b.id WHERE a.k = 10",
    );
    assert_eq!(comma, on_form, "one query, one plan");
    assert!(
        comma.iter().any(|l| l.contains("Hash Join")),
        "expected a Hash Join: {comma:?}"
    );
    assert!(
        comma.iter().any(|l| l.contains("Hash Cond: (a.id = b.id)")),
        "the promoted conjunct is the join condition: {comma:?}"
    );
    assert!(
        comma
            .iter()
            .filter(|l| l.contains("Filter:"))
            .all(|l| !l.contains("a.id = b.id")),
        "and is not repeated as a scan filter: {comma:?}"
    );
    assert!(
        comma.iter().any(|l| l.contains("Filter: (a.k = 10)")),
        "the side predicate stays on the scan: {comma:?}"
    );
    // Nothing to promote — still a nested loop, still says so.
    let non_eq = vals(
        &mut e,
        "EXPLAIN SELECT count(*) FROM t1 a, t2 b WHERE a.id < b.id",
    );
    assert!(
        non_eq.iter().any(|l| l.contains("Nested Loop")),
        "{non_eq:?}"
    );
    // An outer join keeps its own ON as the hash condition.
    let outer = vals(
        &mut e,
        "EXPLAIN SELECT count(*) FROM t1 a LEFT JOIN t2 b ON a.k = b.k WHERE a.id = b.id",
    );
    assert!(
        outer.iter().any(|l| l.contains("Hash Cond: (a.k = b.k)")),
        "{outer:?}"
    );
}

/// Aggregates read the same rows, and at a size where the peer is scanned
/// rather than crossed the answer must not change either.
#[test]
fn round588_aggregates_and_scale() {
    let mut e = seed();
    assert_eq!(
        vals(
            &mut e,
            "SELECT count(*), sum(a.id) FROM t1 a, t2 b WHERE a.k = b.k"
        ),
        vec!["4|10"]
    );
    let mut e = Engine::new();
    e.execute("CREATE TABLE big (id INT, g INT)").unwrap();
    e.execute("INSERT INTO big SELECT gg, gg % 50 FROM generate_series(1, 20000) gg")
        .unwrap();
    assert_eq!(
        one(
            &mut e,
            "SELECT count(*) FROM big a, big b WHERE a.id = b.id AND a.id < 100 AND b.id < 100"
        ),
        "99"
    );
    assert_eq!(
        one(
            &mut e,
            "SELECT count(*) FROM big a, big b WHERE a.id = b.id"
        ),
        "20000"
    );
    // The g key repeats 400 times over 50 values, so the product is
    // 50 * 400 * 400 — a promoted key that lost duplicates would show here.
    assert_eq!(
        one(&mut e, "SELECT count(*) FROM big a, big b WHERE a.g = b.g"),
        "8000000"
    );
    assert_eq!(
        one(
            &mut e,
            "SELECT count(*) FROM big a, big b WHERE a.g = b.g AND a.id < 100 AND b.id < 100"
        ),
        "197",
        "99 rows a side: g = 0 has one member, the other 49 have two"
    );
}
