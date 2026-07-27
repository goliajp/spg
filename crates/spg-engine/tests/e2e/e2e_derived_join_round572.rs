//! v7.39 (round 572) — an uncorrelated derived table in a join ran once
//! per LEFT ROW.
//!
//! `JOIN (SELECT … FROM t WHERE …) b ON …` is as common a shape as SQL
//! has. The join executor sent every derived table down the LATERAL
//! path, whose contract is to re-run the inner SELECT for each row of
//! the driving side, substituting outer columns first. For a derived
//! table that references nothing outer, that is the same answer computed
//! again and again. Measured on a 500k table:
//!
//!     the derived table alone                    34.7 ms
//!     … as a join peer, 500 rows              8,552 ms
//!     … 2000 rows                            32,610 ms
//!     … 20000 rows                          >120,000 ms (cancelled)
//!     PG18, that same 20000-row query            15.8 ms
//!
//! Linear in the LEFT side, because the inner SELECT ran once per left
//! row. After:
//!
//!     500 rows    54.9 ms      2000  69.1 ms      20000  70.6 ms
//!     PG18 13.7                      11.5                12.6
//!
//! and 10,573 -> 71.3 ms with the derived table on the left instead.
//! Same answers throughout. PG still leads ~5x; this is the difference
//! between unusable and comparable.
//!
//! The gate was `is_constant_values_derived` — only a literal VALUES
//! list was materialised once — while the comment beside it already said
//! the rule should be "only genuinely correlated laterals need
//! per-left-row evaluation". Widening it to that rule took two tries and
//! the gates caught both:
//!
//!   * `select_is_correlated` alone let fifteen lateral tests through. It
//!     answers about columns in the projection, the WHERE and nested
//!     subqueries — it does NOT see an outer reference carried in a
//!     set-returning function's ARGUMENTS, and `LATERAL
//!     generate_series(1, lo.n)` parses into a synthesised SELECT whose
//!     correlation lives there.
//!   * Excluding FROM items with `unnest_expr` / `generate_series_args`
//!     left seven more: a set-returning FUNCTION reads as a NAMED FROM
//!     item, so `LATERAL f(t.col)`, `jsonb_each_text(t.j)` and
//!     `json_table(…)` all looked like plain tables.
//!
//! What tells them apart is resolving the name against the catalog. A
//! derived table over stored tables materialises once; anything else
//! keeps the per-left-row path it needs.

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

fn engine() -> Engine {
    let mut e = Engine::new();
    e.execute("CREATE TABLE d572 (id INT, g INT, t TEXT)").unwrap();
    e.execute("INSERT INTO d572 SELECT gg, gg % 5, 'r' || gg FROM generate_series(1, 200) gg")
        .unwrap();
    e
}

/// The derived form and the plain form must agree, on either side of the
/// join and through the join kinds.
#[test]
fn round572_derived_join_agrees_with_the_plain_form() {
    let mut e = engine();
    let plain = vals(
        &mut e,
        "SELECT count(*) FROM d572 a JOIN d572 b ON a.id = b.id WHERE a.id <= 50 AND b.id <= 50",
    );
    assert_eq!(plain, vec!["50"]);
    assert_eq!(
        vals(
            &mut e,
            "SELECT count(*) FROM d572 a JOIN (SELECT id FROM d572 WHERE id <= 50) b \
             ON a.id = b.id WHERE a.id <= 50"
        ),
        plain,
        "derived on the right"
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT count(*) FROM (SELECT id FROM d572 WHERE id <= 50) b JOIN d572 a \
             ON a.id = b.id WHERE a.id <= 50"
        ),
        plain,
        "derived on the left"
    );
    // A LEFT join keeps its unmatched rows.
    assert_eq!(
        vals(
            &mut e,
            "SELECT count(*) FROM d572 a LEFT JOIN (SELECT id FROM d572 WHERE id <= 50) b \
             ON a.id = b.id"
        ),
        vec!["200"]
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT count(*) FROM d572 a LEFT JOIN (SELECT id FROM d572 WHERE id <= 50) b \
             ON a.id = b.id WHERE b.id IS NULL"
        ),
        vec!["150"]
    );
    // Values from the derived side must come through, not just counts.
    assert_eq!(
        vals(
            &mut e,
            "SELECT a.id, b.s FROM d572 a JOIN (SELECT id, t AS s FROM d572 WHERE id <= 3) b \
             ON a.id = b.id ORDER BY a.id"
        ),
        vec!["1|r1", "2|r2", "3|r3"]
    );
    // `AS y(col…)` renames positionally on the materialised path too.
    assert_eq!(
        vals(
            &mut e,
            "SELECT y.n FROM d572 a JOIN (SELECT id FROM d572 WHERE id <= 2) AS y(n) \
             ON a.id = y.n ORDER BY y.n"
        ),
        vec!["1", "2"]
    );
}

/// A derived table that DOES reference the outer row still runs per
/// left row — materialising it once would answer the first row's
/// question for every row.
#[test]
fn round572_correlated_laterals_still_run_per_row() {
    let mut e = engine();
    // The classic shape: the inner WHERE names an outer column.
    assert_eq!(
        vals(
            &mut e,
            "SELECT a.id, x.c FROM d572 a JOIN LATERAL \
             (SELECT count(*) AS c FROM d572 z WHERE z.id <= a.id) x ON true \
             WHERE a.id <= 3 ORDER BY a.id"
        ),
        vec!["1|1", "2|2", "3|3"],
        "a materialise-once would answer 1 for every row"
    );
    // A set-returning function whose argument is an outer column — the
    // correlation the column check cannot see.
    assert_eq!(
        vals(
            &mut e,
            "SELECT a.id, s.g FROM d572 a, LATERAL generate_series(1, a.g) AS s(g) \
             WHERE a.id <= 4 ORDER BY a.id, s.g"
        ),
        vec!["1|1", "2|1", "2|2", "3|1", "3|2", "3|3", "4|1", "4|2", "4|3", "4|4"]
    );
}

/// A constant VALUES list was the only shape the old gate recognised,
/// and it must keep working.
#[test]
fn round572_values_derived_still_cross_joins() {
    let mut e = engine();
    assert_eq!(
        vals(
            &mut e,
            "SELECT a.id, v.c FROM d572 a JOIN (VALUES ('x'), ('y')) AS v(c) ON true \
             WHERE a.id <= 2 ORDER BY a.id, v.c"
        ),
        vec!["1|x", "1|y", "2|x", "2|y"],
        "every left row pairs with every VALUES row"
    );
}

/// The derived body may itself be non-trivial — a GROUP BY, its own
/// join, an ORDER BY with a LIMIT — and is still computed once.
#[test]
fn round572_nontrivial_derived_bodies() {
    let mut e = engine();
    assert_eq!(
        vals(
            &mut e,
            "SELECT count(*) FROM d572 a JOIN \
             (SELECT g, count(*) AS n FROM d572 GROUP BY g) b ON a.g = b.g"
        ),
        vec!["200"]
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT b.g, b.n FROM d572 a JOIN \
             (SELECT g, count(*) AS n FROM d572 GROUP BY g) b ON a.g = b.g \
             WHERE a.id = 7 ORDER BY b.g"
        ),
        vec!["2|40"]
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT count(*) FROM d572 a JOIN \
             (SELECT id FROM d572 ORDER BY id DESC LIMIT 5) b ON a.id = b.id"
        ),
        vec!["5"]
    );
    // A derived table over a join of stored tables.
    assert_eq!(
        vals(
            &mut e,
            "SELECT count(*) FROM d572 a JOIN \
             (SELECT p.id FROM d572 p JOIN d572 q ON p.id = q.id WHERE p.id <= 10) b \
             ON a.id = b.id"
        ),
        vec!["10"]
    );
}
