//! v7.39 (round 621) — a set-returning projection worked over one kind of
//! source and no other.
//!
//! `SELECT unnest(ARRAY[1,2]), x FROM (VALUES (3),(4)) v(x)` answered
//! `function unnest(integer[]) does not exist`, and so did the same query over
//! a derived table, over `generate_series(…)`, and over `ROWS FROM (…)`. PG
//! answers all of them. Over `FROM unnest(…)` — and over a real table — it
//! worked, which is why it had gone unnoticed.
//!
//! Three near-copies of the same tail materialise a row set and then run the
//! rest of the SELECT over it: the `FROM unnest(…)` executor, the
//! `FROM generate_series(…)` one, and the one that serves VALUES, a derived
//! table and `ROWS FROM (…)`. Only the first knew that a target list can
//! expand. The expansion and the sort-key rule are now one function each,
//! shared by all three — a fourth copy would only have been the fourth place
//! to forget.
//!
//! The sort-key rule is round 600's, and round 621 had just had to apply it in
//! the first of these tails: a key naming a select-list item reads it out of
//! the EXPANDED row, because PG sorts after the expansion; a key naming a
//! source column the query does not project is evaluated against the input row
//! that output row came from.
//!
//! The generate_series tail also sorted by `stmt.order_by` directly rather than
//! resolving positional keys, so `ORDER BY 1` there was the constant 1 — the
//! same sort key for every row. It now resolves them, as the other two always
//! did.
//!
//! Measured and NOT closed (checklist F05b-agg): a target-list SRF beside an
//! AGGREGATE — `SELECT unnest(ARRAY[1,2]), count(*) FROM t` — still answers
//! `function unnest(integer[]) does not exist`. PG expands the SRF over the
//! aggregate's output row; that is the aggregate executor's own gap, not one
//! of these tails'.
//!
//! Every shape below was checked against live PG18 and matches byte for byte.

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

/// Each source, with and without the clause that made round 621's first half
/// necessary.
#[test]
fn round621_every_source_expands_a_target_list_srf() {
    let mut e = Engine::new();
    for src in [
        "(VALUES (3),(4)) v(x)",
        "(SELECT 3 AS x UNION ALL SELECT 4) v",
        "ROWS FROM (generate_series(3,4)) AS v(x)",
    ] {
        assert_eq!(
            vals(&mut e, &format!("SELECT unnest(ARRAY[1,2]), x FROM {src}")),
            vec!["1|3", "2|3", "1|4", "2|4"],
            "source: {src}"
        );
        assert_eq!(
            vals(
                &mut e,
                &format!("SELECT unnest(ARRAY[1,2]), x FROM {src} ORDER BY 1,2")
            ),
            vec!["1|3", "1|4", "2|3", "2|4"],
            "source: {src}, sorted"
        );
    }
    assert_eq!(
        vals(
            &mut e,
            "SELECT unnest(ARRAY[1,2]), g FROM generate_series(3,4) g"
        ),
        vec!["1|3", "2|3", "1|4", "2|4"]
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT unnest(ARRAY[1,2]), g FROM generate_series(3,4) g ORDER BY 1,2"
        ),
        vec!["1|3", "1|4", "2|3", "2|4"]
    );
}

/// The positional key the generate_series tail used to read as a constant.
#[test]
fn round621_generate_series_resolves_a_positional_key() {
    let mut e = Engine::new();
    assert_eq!(
        vals(&mut e, "SELECT g FROM generate_series(3,1,-1) g ORDER BY 1"),
        vec!["1", "2", "3"],
        "`ORDER BY 1` means the first OUTPUT column, not the literal 1"
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT g FROM generate_series(1,3) g ORDER BY 1 DESC"
        ),
        vec!["3", "2", "1"]
    );
}

/// The sources that already worked, and the rest of the clause list.
#[test]
fn round621_the_sources_that_already_worked() {
    let mut e = Engine::new();
    assert_eq!(
        vals(
            &mut e,
            "SELECT unnest(ARRAY[1,2]), y FROM unnest(ARRAY[5,6]) y ORDER BY 1,2"
        ),
        vec!["1|5", "1|6", "2|5", "2|6"]
    );
    e.execute("CREATE TABLE t1 (x INT)").unwrap();
    e.execute("INSERT INTO t1 VALUES (4),(3)").unwrap();
    assert_eq!(
        vals(&mut e, "SELECT unnest(ARRAY[1,2]), x FROM t1 ORDER BY 2,1"),
        vec!["1|3", "2|3", "1|4", "2|4"]
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT unnest(ARRAY[1,2]), x FROM (VALUES (3),(4)) v(x) ORDER BY 1 LIMIT 3"
        ),
        vec!["1|3", "1|4", "2|3"],
        "LIMIT counts the expanded rows"
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT DISTINCT unnest(ARRAY[1,1,2]) FROM (VALUES (3),(4)) v(x) ORDER BY 1"
        ),
        vec!["1", "2"]
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT unnest(ARRAY[1,2]) FROM (VALUES (3),(4)) v(x) WHERE x = 4"
        ),
        vec!["1", "2"],
        "WHERE filters the SOURCE rows, before the expansion"
    );
}
