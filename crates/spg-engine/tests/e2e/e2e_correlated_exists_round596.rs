//! v7.39 (round 596) — a correlated EXISTS stopped decorrelating the moment
//! the outer side stopped being a bare column.
//!
//! Every remaining item on the ledger's list was about 2x, so this round
//! swept a surface that had not been swept — aggregates, CTEs, JSON, arrays,
//! subqueries — the way round 589 found its 4800x. The sweep's worst by a
//! wide margin:
//!
//!     EXISTS (SELECT 1 FROM d b WHERE b.id = a.id + 1)   >20 s   PG 31.4 ms
//!
//! It is quadratic, and the neighbouring shape is not. Holding everything
//! else and varying only the row count:
//!
//!     rows    b.id = a.id + 1        b.id = a.id
//!      2000    427.01 ->  2.07 ms      0.55 -> 0.41
//!      4000   1643.70 ->  3.09         0.90 -> 0.56
//!      8000   6379.59 ->  5.88         1.47 -> 0.90
//!     16000  25003.91 -> 11.14         3.16 -> 1.68   (25 s was a timeout)
//!
//! `try_batch_correlated_exists` turns the subquery into one scan plus a
//! membership test per outer row — PG's Hash Semi Join — but it recognised a
//! correlation only as `<inner column> = <outer column>`, storing the outer
//! side as a `ColumnName`. `a.id + 1` fell off that shape, and with it the
//! whole rewrite, so the subquery ran once per outer row. On 500k rows the
//! same query goes from a >20-second timeout to 332 ms against PG's 31.4:
//! still a loss, no longer a different complexity class.
//!
//! The outer side is an expression now. Two things decide whether one
//! qualifies, both taken from round 590, which fixed this exact shape for
//! JOIN keys:
//!
//! **What it may contain** — an allowlist of node kinds (columns, literals,
//! casts, unary, arithmetic) rather than "does it only mention outer
//! columns". A node the walk did not know about, or a function whose
//! volatility SPG cannot look up, would both be admitted silently, and a
//! volatile key would probe the wrong bucket. `lower(a.s)` therefore stays
//! per-row, and E11 below is that case.
//!
//! **How it encodes** — the set is built from the INNER column's values and
//! probed with the OUTER expression's, and arithmetic widens: `a.b - 1` over
//! a BIGINT need not carry the same tag as the INT column it is compared to.
//! Both sides now encode through the canonical numeric form round 590
//! introduced, which also closes that hole for the column-to-column shape
//! that was already decorrelating.
//!
//! All 20 shapes here were checked against live PG18 and matched.

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
    for t in ["ea", "eb"] {
        e.execute(&format!(
            "CREATE TABLE {t} (id INT, k INT, b BIGINT, n NUMERIC(10,2), s TEXT)"
        ))
        .unwrap();
    }
    e.execute(
        "INSERT INTO ea VALUES (1,1,10,1.00,'a'),(2,1,20,2.50,'b'),(3,2,NULL,NULL,NULL),\
         (4,2,0,0.00,'d'),(5,3,-3,1.50,'e')",
    )
    .unwrap();
    e.execute(
        "INSERT INTO eb VALUES (0,1,9,4.00,'a'),(1,1,19,1.50,'b'),(2,2,NULL,NULL,NULL),\
         (3,2,-1,-1.00,'d'),(4,3,-4,0.50,'e'),(9,9,99,9.00,'z')",
    )
    .unwrap();
    e
}

/// The shape the round is about, both spellings and both polarities.
#[test]
fn round596_expression_correlations() {
    let mut e = seed();
    assert_eq!(
        vals(
            &mut e,
            "SELECT id FROM ea a WHERE EXISTS (SELECT 1 FROM eb b WHERE b.id = a.id + 1) \
             ORDER BY id"
        ),
        vec!["1", "2", "3"]
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT id FROM ea a WHERE EXISTS (SELECT 1 FROM eb b WHERE a.id + 1 = b.id) \
             ORDER BY id"
        ),
        vec!["1", "2", "3"],
        "written the other way round"
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT id FROM ea a WHERE NOT EXISTS (SELECT 1 FROM eb b WHERE b.id = a.id + 1) \
             ORDER BY id"
        ),
        vec!["4", "5"],
        "the anti-join half"
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT id FROM ea a WHERE EXISTS (SELECT 1 FROM eb b WHERE b.id = a.id) ORDER BY id"
        ),
        vec!["1", "2", "3", "4"],
        "the plain column shape, which already worked"
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT id FROM ea a WHERE EXISTS \
             (SELECT 1 FROM eb b WHERE b.k = a.k AND b.id = a.id - 1) ORDER BY id"
        ),
        vec!["1", "2", "3", "4", "5"],
        "one column correlation and one expression correlation together"
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT id FROM ea a WHERE EXISTS (SELECT 1 FROM eb b WHERE b.id = a.id / 2) \
             ORDER BY id"
        ),
        vec!["1", "2", "3", "4", "5"]
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT id FROM ea a WHERE EXISTS (SELECT 1 FROM eb b WHERE b.id = (a.n)::INT) \
             ORDER BY id"
        ),
        vec!["1", "2", "4", "5"],
        "a cast in the outer expression"
    );
}

/// The set is built from one side's values and probed with the other's, so
/// values that compare equal must encode alike across the numeric family.
#[test]
fn round596_keys_meet_across_numeric_widths() {
    let mut e = seed();
    assert_eq!(
        vals(
            &mut e,
            "SELECT id FROM ea a WHERE EXISTS (SELECT 1 FROM eb b WHERE b.b = a.b - 1) ORDER BY id"
        ),
        vec!["1", "2", "4", "5"],
        "BIGINT arithmetic against a BIGINT column"
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT id FROM ea a WHERE EXISTS (SELECT 1 FROM eb b WHERE b.id = a.n + 1) ORDER BY id"
        ),
        vec!["1", "4"],
        "an INT column probed with a NUMERIC expression"
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT id FROM ea a WHERE EXISTS (SELECT 1 FROM eb b WHERE b.s = a.s) ORDER BY id"
        ),
        vec!["1", "2", "4", "5"],
        "text keys are untouched by the numeric canonicalisation"
    );
}

/// A NULL key can never satisfy `=`, on either side.
#[test]
fn round596_nulls_match_nothing() {
    let mut e = seed();
    assert!(
        vals(
            &mut e,
            "SELECT id FROM ea a WHERE EXISTS (SELECT 1 FROM eb b WHERE b.b = a.b + 0) ORDER BY id"
        )
        .is_empty(),
        "no outer b + 0 lands on an inner b here, and the NULL rows join nothing"
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT count(*) FROM ea a WHERE EXISTS (SELECT 1 FROM eb b WHERE b.n = a.n * 1)"
        ),
        vec!["1"]
    );
}

/// The shapes that must NOT take the rewrite, and still answer the same.
#[test]
fn round596_ineligible_shapes_keep_per_row_execution() {
    let mut e = seed();
    assert_eq!(
        vals(
            &mut e,
            "SELECT id FROM ea a WHERE EXISTS (SELECT 1 FROM eb b WHERE b.s = lower(a.s)) \
             ORDER BY id"
        ),
        vec!["1", "2", "4", "5"],
        "a function on the outer side is not admitted"
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT id FROM ea a WHERE EXISTS (SELECT 1 FROM eb b WHERE b.id + 1 = a.id) \
             ORDER BY id"
        ),
        vec!["1", "2", "3", "4", "5"],
        "an expression on the INNER side is not a key — the set is built from it"
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT id FROM ea a WHERE EXISTS (SELECT 1 FROM eb b WHERE b.id + 1 = a.id + 2) \
             ORDER BY id"
        ),
        vec!["1", "2", "3"],
        "both sides computed"
    );
}

/// Where the EXISTS sits changes which path runs it.
#[test]
fn round596_every_position_agrees() {
    let mut e = seed();
    assert_eq!(
        vals(
            &mut e,
            "SELECT id FROM ea a WHERE EXISTS \
             (SELECT 1 FROM eb b WHERE b.id = a.id + 1 AND b.k > 1) ORDER BY id"
        ),
        vec!["1", "2", "3"],
        "an extra inner filter rides into the build scan"
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT id FROM ea a WHERE a.id = 5 OR EXISTS \
             (SELECT 1 FROM eb b WHERE b.id = a.id + 1) ORDER BY id"
        ),
        vec!["1", "2", "3", "5"]
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT id FROM ea a WHERE EXISTS (SELECT 1 FROM eb b WHERE b.id = a.id + 1) \
             AND NOT EXISTS (SELECT 1 FROM eb c WHERE c.id = a.id + 4) ORDER BY id"
        ),
        vec!["1", "2", "3"],
        "two of them, one negated"
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT id, EXISTS (SELECT 1 FROM eb b WHERE b.id = a.id + 1) FROM ea a ORDER BY id"
        ),
        vec!["1|true", "2|true", "3|true", "4|false", "5|false"],
        "projected rather than filtered"
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT id FROM ea a WHERE EXISTS \
             (SELECT 1 FROM eb b JOIN eb c ON b.id = c.id WHERE b.id = a.id + 1) ORDER BY id"
        ),
        vec!["1", "2", "3"],
        "the inner relation may itself be a join"
    );
}

/// At a size where the old path was quadratic, the answer has to be the one
/// the per-row path gives.
#[test]
fn round596_scale_agrees_with_per_row() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE big (id INT, g INT)").unwrap();
    e.execute("INSERT INTO big SELECT gg, gg % 10 FROM generate_series(1, 4000) gg")
        .unwrap();
    // Every id but the last has a successor.
    assert_eq!(
        vals(
            &mut e,
            "SELECT count(*) FROM big a WHERE EXISTS (SELECT 1 FROM big b WHERE b.id = a.id + 1)"
        ),
        vec!["3999"]
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT count(*) FROM big a WHERE NOT EXISTS \
             (SELECT 1 FROM big b WHERE b.id = a.id + 1)"
        ),
        vec!["1"]
    );
    // The same question asked a way that cannot be decorrelated must agree.
    assert_eq!(
        vals(
            &mut e,
            "SELECT count(*) FROM big a WHERE EXISTS \
             (SELECT 1 FROM big b WHERE b.id - 1 = a.id)"
        ),
        vec!["3999"]
    );
}
