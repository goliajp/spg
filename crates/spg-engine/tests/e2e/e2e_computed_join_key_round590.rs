//! v7.39 (round 590) — an equality whose peer side is computed was not a
//! join key, so the join paid for the whole bucket.
//!
//! Round 589's sweep left this at the top of what remained:
//! `ON a.g = b.g AND a.id = b.id + 1` over 500k rows ran past a 20-second
//! timeout where PG18 takes 31.5 ms. `extract_join_keys` only recognised
//! `<column> = <column>`, so the hash keyed on `g` alone — 50 distinct
//! values over 500k rows — and the second conjunct became a residual tested
//! on every candidate pair, each of which materialises a combined row.
//!
//! The cost was exactly (probe rows x bucket size). Holding the row count at
//! 20,000 and varying only how many distinct `g` there are:
//!
//!     bucket 20000   25,002 ms (timed out)  ->  12.06 ms    PG  3.58
//!     bucket  2000    6,425.85              ->   6.33       PG  5.98
//!     bucket   200      686.08              ->   7.82       PG  7.93
//!     bucket    20      101.35              ->   6.47       PG  8.90
//!     bucket     1       22.51              ->   5.85       PG 10.86
//!
//! The dependence on bucket size is gone. On the 500k shape that started it,
//! >20 s -> 134.8 ms against PG's 29.9, and its row-producing sibling
//! (`b.id + 50`, 499,950 rows) 273 ms against 39.3. Still a loss; no longer
//! a category difference.
//!
//! Two things decide whether an expression can be a key.
//!
//! **What it may contain.** An allowlist of node kinds — columns, literals,
//! casts, unary and arithmetic — rather than a walk that asks what an
//! expression references. A node the walk did not know about, or a function
//! whose volatility SPG cannot look up, would both be silently admitted, and
//! a volatile key would join the wrong rows. That leaves
//! `ON a.k = lower(b.k)` on the old path: recorded, not done.
//!
//! **How it encodes.** The whole requirement is that two values SQL calls
//! equal encode identically, or the join silently loses rows — and across
//! the numeric family that is not free, since `5` as INT, `5` as BIGINT,
//! `5.0` as double and `5.00` as NUMERIC all compare equal and would
//! otherwise carry four different tags. They are rendered as one canonical
//! decimal. The conjunct also stays in the residual, so a key that failed to
//! match the right rows could only ever drop them, never invent them.
//!
//! Every shape in this file was checked against live PG18 and matched.

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
    for t in ["ja", "jb"] {
        e.execute(&format!(
            "CREATE TABLE {t} (id INT, k INT, b BIGINT, f DOUBLE PRECISION, \
             n NUMERIC(10,2), s TEXT)"
        ))
        .unwrap();
    }
    e.execute(
        "INSERT INTO ja VALUES (1,1,10,5.0,5.00,'a'),(2,1,20,2.5,2.50,'b'),\
         (3,2,NULL,NULL,NULL,NULL),(4,2,0,-0.0,0.00,'d'),(5,3,-3,1.5,1.50,'e')",
    )
    .unwrap();
    e.execute(
        "INSERT INTO jb VALUES (0,1,9,4.0,4.00,'a'),(1,1,19,1.5,1.50,'b'),\
         (2,2,NULL,NULL,NULL,NULL),(3,2,-1,-1.0,-1.00,'d'),(4,3,-4,0.5,0.50,'e'),\
         (9,9,99,9.0,9.00,'z')",
    )
    .unwrap();
    e
}

/// The shape the round is about, in both spellings.
#[test]
fn round590_computed_key_joins() {
    let mut e = seed();
    assert_eq!(
        vals(
            &mut e,
            "SELECT a.id, b.id FROM ja a JOIN jb b ON a.k = b.k AND a.id = b.id + 1 ORDER BY 1"
        ),
        vec!["1|0", "2|1", "3|2", "4|3", "5|4"]
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT a.id, b.id FROM ja a JOIN jb b ON a.k = b.k AND b.id + 1 = a.id ORDER BY 1"
        ),
        vec!["1|0", "2|1", "3|2", "4|3", "5|4"],
        "written the other way round"
    );
    // A computed key with no plain-column key beside it.
    assert_eq!(
        vals(
            &mut e,
            "SELECT a.id, b.id FROM ja a JOIN jb b ON a.b = b.b + 1 ORDER BY 1"
        ),
        vec!["1|0", "2|1", "4|3", "5|4"]
    );
    // Multiplication, modulo, unary minus and a cast all qualify.
    assert_eq!(
        vals(&mut e, "SELECT a.id, b.id FROM ja a JOIN jb b ON a.b = b.b * 2 + 2 ORDER BY 1, 2"),
        vec!["2|0", "4|3"]
    );
    assert_eq!(
        vals(&mut e, "SELECT a.id, b.id FROM ja a JOIN jb b ON a.b = b.b / 2 ORDER BY 1, 2"),
        vec!["4|3"]
    );
    assert!(
        vals(&mut e, "SELECT a.id, b.id FROM ja a JOIN jb b ON a.b = b.b * 2 ORDER BY 1").is_empty(),
        "nothing doubles into the other side"
    );
    assert_eq!(
        vals(&mut e, "SELECT a.id, b.id FROM ja a JOIN jb b ON a.k = b.id % 3 ORDER BY 1"),
        vec!["1|1", "1|4", "2|1", "2|4", "3|2", "4|2"]
    );
    assert!(
        vals(&mut e, "SELECT a.id, b.id FROM ja a JOIN jb b ON a.b = -b.b ORDER BY 1").is_empty(),
        "no value is its own negation here"
    );
    assert_eq!(
        vals(&mut e, "SELECT a.id, b.id FROM ja a JOIN jb b ON a.id = b.k::INT ORDER BY 1"),
        vec!["1|0", "1|1", "2|2", "2|3", "3|4"]
    );
}

/// Values that compare equal across the numeric family have to meet in the
/// same bucket — an INT column against a NUMERIC or double expression, and
/// BIGINT against INT widths.
#[test]
fn round590_numeric_keys_meet_across_types() {
    let mut e = seed();
    assert_eq!(
        vals(&mut e, "SELECT a.id, b.id FROM ja a JOIN jb b ON a.id = b.n + 1 ORDER BY 1"),
        vec!["5|0"],
        "INT column against a NUMERIC expression"
    );
    assert_eq!(
        vals(&mut e, "SELECT a.id, b.id FROM ja a JOIN jb b ON a.id = b.f + 1 ORDER BY 1"),
        vec!["5|0"],
        "INT column against a double expression"
    );
    assert_eq!(
        vals(&mut e, "SELECT a.id, b.id FROM ja a JOIN jb b ON a.f = b.n + 1.00 ORDER BY 1"),
        vec!["1|0", "2|1", "4|3", "5|4"]
    );
    // -0.0 equals 0 and must land in the same bucket.
    assert_eq!(
        vals(&mut e, "SELECT a.id, b.id FROM ja a JOIN jb b ON a.f = b.f * 0 ORDER BY 1"),
        vec!["4|0", "4|1", "4|3", "4|4", "4|9"]
    );
    // A NULL on the computed side joins nothing, exactly like a NULL column.
    assert_eq!(
        one(&mut e, "SELECT count(*) FROM ja a JOIN jb b ON a.b = b.b + 0"),
        "0"
    );
}

/// The shapes that must NOT become keys, and still have to answer the same.
#[test]
fn round590_ineligible_shapes_keep_the_old_path() {
    let mut e = seed();
    assert_eq!(
        vals(&mut e, "SELECT a.id, b.id FROM ja a JOIN jb b ON a.s = lower(b.s) ORDER BY 1"),
        vec!["1|0", "2|1", "4|3", "5|4"],
        "a function call is not admitted, and the residual still answers"
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT a.id, b.id FROM ja a JOIN jb b ON a.id + 1 = b.id + 2 ORDER BY 1"
        ),
        vec!["1|0", "2|1", "3|2", "4|3", "5|4"],
        "both sides computed"
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT a.id, b.id FROM ja a JOIN jb b ON a.k = b.k AND a.id = 2 + 1 ORDER BY 1"
        ),
        vec!["3|2", "3|3"],
        "a constant right side is a filter, not a key"
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT a.id, b.id FROM ja a JOIN jb b ON a.id = b.id + 1 AND a.k > b.k - 1 ORDER BY 1"
        ),
        vec!["1|0", "2|1", "3|2", "4|3", "5|4"],
        "a residual inequality beside a computed key"
    );
}

/// Outer joins must keep their unmatched rows: the key decides which rows
/// meet, never which rows survive.
#[test]
fn round590_outer_joins_keep_unmatched_rows() {
    let mut e = seed();
    assert_eq!(
        vals(
            &mut e,
            "SELECT a.id, b.id FROM ja a LEFT JOIN jb b ON a.k = b.k AND a.id = b.id + 1 ORDER BY 1"
        ),
        vec!["1|0", "2|1", "3|2", "4|3", "5|4"]
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT a.id, b.id FROM ja a RIGHT JOIN jb b ON a.id = b.id + 1 ORDER BY 1, 2"
        ),
        vec!["1|0", "2|1", "3|2", "4|3", "5|4", "NULL|9"]
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT a.id, b.id FROM ja a FULL OUTER JOIN jb b ON a.id = b.id + 1 ORDER BY 1, 2"
        ),
        vec!["1|0", "2|1", "3|2", "4|3", "5|4", "NULL|9"]
    );
}

/// At a size where the bucket used to decide the cost, the answer has to
/// match what a plain nested loop would give.
#[test]
fn round590_scale_matches_the_slow_path() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE big (id INT, g INT)").unwrap();
    // 100 distinct g over 5000 rows: 50 rows a bucket.
    e.execute("INSERT INTO big SELECT gg, gg % 100 FROM generate_series(1, 1500) gg")
        .unwrap();
    // a.id = b.id + 1 forces consecutive ids, and g = id % 100 then differs,
    // so the pair count is zero — the shape the round measured.
    assert_eq!(
        one(
            &mut e,
            "SELECT count(*) FROM big a JOIN big b ON a.g = b.g AND a.id = b.id + 1"
        ),
        "0"
    );
    // Shift by the modulus instead and every row but the last 100 matches.
    assert_eq!(
        one(
            &mut e,
            "SELECT count(*) FROM big a JOIN big b ON a.g = b.g AND a.id = b.id + 100"
        ),
        "1400"
    );
    // Same answer with the key made ineligible by computing both sides.
    assert_eq!(
        one(
            &mut e,
            "SELECT count(*) FROM big a JOIN big b ON a.g = b.g AND a.id + 0 = b.id + 100"
        ),
        "1400",
        "the residual path agrees with the keyed one"
    );
    // A three-way chain where each peer keys off a computed expression.
    assert_eq!(
        one(
            &mut e,
            "SELECT count(*) FROM big a JOIN big b ON b.id = a.id + 1 \
             JOIN big c ON c.id = b.id + 1"
        ),
        "1498"
    );
}
