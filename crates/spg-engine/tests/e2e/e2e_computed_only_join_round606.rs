//! v7.39 (round 606) — a computed join key ON ITS OWN was not a key at all.
//!
//! Round 590 taught the hash stage to take key EXPRESSIONS, and measured
//! `ON a.g = b.g AND a.id = b.id + 1` from past 25 seconds down to 12 ms. But
//! the gate that decides whether to hash at all only ever asked about
//! `eq_pairs` — the plain `col = col` list — so the new machinery could only
//! be reached when a plain equality happened to sit beside the computed one.
//! With the computed equality ALONE, `eq_pairs` was empty, the peer fell
//! through to the nested loop, and the whole of `b` was crossed against every
//! row of `a`:
//!
//!     rows      ON a.id = b.id      ON a.id = b.id + 1     PG18
//!     2,000            0.77 ms              701.96 ms      0.43-0.46 ms
//!     5,000            1.69              4,455.59          0.78
//!    10,000            3.31             17,537.26          1.35
//!    20,000            6.45          past the 20 s timeout  2.67
//!
//! Quadratic against PG's flat couple of milliseconds — and the shape is not
//! exotic. `a.id = b.id + 1` is the ordinary previous-row join, and
//! `LEFT JOIN … ON a.id = b.id + 1 WHERE b.id IS NULL` is the ordinary
//! anti-join; on the 500k-row table the anti-join ran past 25 seconds against
//! PG's 60 ms. Letting `eq_exprs` open the same gate:
//!
//!     20,000 rows   inner      20,020 (timeout) ->    14.21 ms   PG  2.66
//!     20,000 rows   anti-join  20,000 (timeout) ->    14.82      PG  2.56
//!    500,000 rows   anti-join  25,016 (timeout) ->   275.25      PG 59.91
//!    500,000 rows   inner                       ->   267.50      PG 52.72
//!
//! linear where it was quadratic, and a different complexity class no more.
//!
//! Nothing about the answer rides on this. The computed conjunct is kept in
//! `residual` as well as in the key list (round 590's rule), so a key that
//! encoded badly could only ever drop a candidate pair, never invent one —
//! and the residual re-checks every pair the hash proposes. What the pins
//! below are for is the part that is genuinely new: this gate is the first
//! time an OUTER join can reach the hash stage on a computed key alone, so
//! all four kinds, the NULL key on each side, and the unmatched-row emission
//! that LEFT / RIGHT / FULL owe are pinned. All 22 shapes were checked
//! against live PG18 and matched byte for byte.
//!
//! Recorded and NOT done: `EXPLAIN` printed `Hash Join` for these queries all
//! along — it decides on "is the top-level ON an equality", which was true
//! while the executor was looping. The plan was describing a join the
//! executor did not run (the round 551 / 565 class of defect); it is right
//! now because the executor caught up with it, not because the printer
//! changed.

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
    e.execute("CREATE TABLE ja (id INT, g INT, b BIGINT, n NUMERIC, s TEXT)").unwrap();
    e.execute("CREATE TABLE jb2 (id INT, g INT, b BIGINT, n NUMERIC, s TEXT)").unwrap();
    e.execute(
        "INSERT INTO ja VALUES (1,10,1,1.0,'a'),(2,20,2,2.0,'b'),(3,30,NULL,NULL,NULL),\
         (4,10,4,4.00,'d'),(5,20,5,5.0,'e'),(NULL,10,6,6.0,'f')",
    )
    .unwrap();
    e.execute(
        "INSERT INTO jb2 VALUES (0,10,0,0.0,'z'),(1,20,1,1.00,'a'),(2,30,2,2.0,'b'),\
         (3,10,NULL,NULL,NULL),(4,20,4,4.0,'d'),(NULL,30,9,9.0,'q')",
    )
    .unwrap();
    e
}

/// All four join kinds on a computed key alone. The outer kinds still owe
/// their unmatched rows, and a NULL key matches nothing on either side.
#[test]
fn round606_every_join_kind_on_a_computed_key() {
    let mut e = seed();
    assert_eq!(
        vals(&mut e, "SELECT a.id, b.id FROM ja a JOIN jb2 b ON a.id = b.id + 1 ORDER BY 1,2"),
        vec!["1|0", "2|1", "3|2", "4|3", "5|4"]
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT a.id, b.id FROM ja a LEFT JOIN jb2 b ON a.id = b.id + 1 ORDER BY 1,2"
        ),
        vec!["1|0", "2|1", "3|2", "4|3", "5|4", "NULL|NULL"],
        "the left row whose key is NULL matches nothing and is still emitted"
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT a.id, b.id FROM ja a RIGHT JOIN jb2 b ON a.id = b.id + 1 ORDER BY 1,2"
        ),
        vec!["1|0", "2|1", "3|2", "4|3", "5|4", "NULL|NULL"],
        "and the build-side row whose key expression is NULL never enters a \
         bucket, so RIGHT has to emit it from the unmatched pass"
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT a.id, b.id FROM ja a FULL JOIN jb2 b ON a.id = b.id + 1 ORDER BY 1,2"
        ),
        vec!["1|0", "2|1", "3|2", "4|3", "5|4", "NULL|NULL", "NULL|NULL"],
        "both of them"
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT a.id FROM ja a LEFT JOIN jb2 b ON a.id = b.id + 1 WHERE b.id IS NULL ORDER BY 1"
        ),
        vec!["NULL"],
        "the anti-join: only the NULL-keyed left row survives"
    );
}

/// The arithmetic the key allowlist admits, and the types it has to compare
/// across. A key that encoded 5 and 5.0 differently would silently lose rows.
#[test]
fn round606_key_expression_shapes() {
    let mut e = seed();
    assert_eq!(
        vals(&mut e, "SELECT a.id, b.id FROM ja a JOIN jb2 b ON a.id = b.id - 1 ORDER BY 1,2"),
        vec!["1|2", "2|3", "3|4"]
    );
    assert_eq!(
        vals(&mut e, "SELECT a.id, b.id FROM ja a JOIN jb2 b ON a.id = b.id % 3 ORDER BY 1,2"),
        vec!["1|1", "1|4", "2|2"],
        "several build rows per key"
    );
    assert_eq!(
        vals(&mut e, "SELECT a.id, b.id FROM ja a JOIN jb2 b ON a.id = b.id / 2 ORDER BY 1,2"),
        vec!["1|2", "1|3", "2|4"],
        "integer division"
    );
    assert_eq!(
        vals(&mut e, "SELECT a.id, b.id FROM ja a JOIN jb2 b ON a.id = -b.id ORDER BY 1,2"),
        Vec::<String>::new(),
        "unary minus, matching nothing"
    );
    assert_eq!(
        vals(&mut e, "SELECT a.id, b.id FROM ja a JOIN jb2 b ON a.b = b.id + 1 ORDER BY 1,2"),
        vec!["1|0", "2|1", "4|3", "5|4"],
        "BIGINT probe against an INT key expression"
    );
    assert_eq!(
        vals(&mut e, "SELECT a.id, b.id FROM ja a JOIN jb2 b ON a.id = b.b + 1 ORDER BY 1,2"),
        vec!["1|0", "2|1", "3|2", "5|4"],
        "and the reverse"
    );
    assert_eq!(
        vals(&mut e, "SELECT a.id, b.id FROM ja a JOIN jb2 b ON a.n = b.n + 1 ORDER BY 1,2"),
        vec!["1|0", "2|1", "5|4"],
        "NUMERIC, where 1.0 and 1.00 are the same value"
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT a.id, b.id FROM ja a JOIN jb2 b ON a.id = (b.id + 1)::BIGINT ORDER BY 1,2"
        ),
        vec!["1|0", "2|1", "3|2", "4|3", "5|4"],
        "a cast around the key"
    );
    assert_eq!(
        vals(&mut e, "SELECT count(*) FROM ja a JOIN jb2 b ON a.g = b.g + 0"),
        vec!["12"],
        "a key with many rows a bucket"
    );
}

/// A computed key beside other conjuncts — plain equality, another computed
/// equality, and a non-equality that stays residual.
#[test]
fn round606_computed_key_beside_others() {
    let mut e = seed();
    assert_eq!(
        vals(
            &mut e,
            "SELECT a.id, b.id FROM ja a JOIN jb2 b ON a.id = b.id + 1 AND a.g = b.g ORDER BY 1,2"
        ),
        vec!["1|0", "2|1", "3|2", "4|3", "5|4"]
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT a.id, b.id FROM ja a JOIN jb2 b ON a.id = b.id + 1 AND a.g = b.g * 1 ORDER BY 1,2"
        ),
        vec!["1|0", "2|1", "3|2", "4|3", "5|4"],
        "two computed keys"
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT a.id, b.id FROM ja a JOIN jb2 b ON a.id = b.id + 1 AND a.g > b.g ORDER BY 1,2"
        ),
        Vec::<String>::new(),
        "a non-equality is residual and rejects every candidate pair"
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT a.id, b.id FROM ja a LEFT JOIN jb2 b ON a.id = b.id + 1 AND b.g = 20 ORDER BY 1,2"
        ),
        vec!["1|NULL", "2|1", "3|NULL", "4|NULL", "5|4", "NULL|NULL"],
        "an ON-clause filter on the build side still NULL-fills, it does not drop"
    );
    assert_eq!(
        vals(&mut e, "SELECT a.s, b.s FROM ja a JOIN jb2 b ON a.s = b.s ORDER BY 1,2"),
        vec!["a|a", "b|b", "d|d"],
        "the plain path is unchanged"
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT a.id FROM ja a WHERE EXISTS (SELECT 1 FROM jb2 b WHERE a.id = b.id + 1) ORDER BY 1"
        ),
        vec!["1", "2", "3", "4", "5"]
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT a.id, b.id, c.id FROM ja a JOIN jb2 b ON a.id = b.id + 1 \
             JOIN ja c ON c.id = b.id ORDER BY 1,2,3"
        ),
        vec!["2|1|1", "3|2|2", "4|3|3", "5|4|4"],
        "chained, with the computed key in the middle"
    );
}

/// At the size where the nested loop was the whole cost. Both spellings have
/// to agree, and the answer has to be the one the slow path gave.
#[test]
fn round606_scale() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE big (id INT, g INT)").unwrap();
    e.execute("INSERT INTO big SELECT gg, gg % 50 FROM generate_series(1, 20000) gg")
        .unwrap();
    assert_eq!(
        vals(&mut e, "SELECT count(*) FROM big a JOIN big b ON a.id = b.id + 1"),
        vec!["19999"]
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT count(*) FROM big a LEFT JOIN big b ON a.id = b.id + 1 WHERE b.id IS NULL"
        ),
        vec!["1"],
        "only id = 1 has no predecessor"
    );
    assert_eq!(
        vals(&mut e, "SELECT count(*) FROM big a JOIN big b ON a.id = b.id + 1 AND a.g = b.g"),
        vals(&mut e, "SELECT count(*) FROM big a JOIN big b ON a.g = b.g AND a.id = b.id + 1"),
        "the order of the conjuncts decides which list a key lands in, never the answer"
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT min(a.id), max(a.id) FROM big a JOIN big b ON a.id = b.id + 1"
        ),
        vec!["2|20000"]
    );
}
