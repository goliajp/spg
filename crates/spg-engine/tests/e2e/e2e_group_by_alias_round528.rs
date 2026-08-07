//! v7.39 (round 528) — GROUP BY an output alias.
//!
//! This round set out to re-measure the twelve v7.37 audit entries left
//! standing, because three had already proved stale on contact. Nine of
//! the fifteen no longer reproduce as written — including the two that
//! led here, `GROUP BY … WITH ROLLUP` (which works, byte for byte
//! against MariaDB 11) and non-strict `sql_mode` (which truncates as
//! MariaDB does).
//!
//! What the ROLLUP probe actually hit was something the audit never
//! listed:
//!
//!     SELECT date_trunc('day', ts) AS d, count(*) FROM t GROUP BY d
//!     PG18  2 rows        SPG  ERROR: column "d" does not exist
//!
//! The canonical daily rollup. Both PG and MySQL take a GROUP BY
//! identifier that names an output alias and group by the expression
//! behind it; SPG accepted only a real column or an ordinal.
//!
//! Every expectation below is a PG18 reading, cross-checked on
//! MariaDB 11 for the dialect-independent shapes.

use spg_engine::{Engine, QueryResult};

fn engine() -> Engine {
    let mut e = Engine::new();
    e.execute("CREATE TABLE ga (ts TIMESTAMP, v INT)").unwrap();
    e.execute(
        "INSERT INTO ga VALUES ('2020-01-01 10:00', 1), \
         ('2020-01-01 20:00', 2), ('2020-01-02 05:00', 5)",
    )
    .unwrap();
    e
}

fn rows(e: &mut Engine, sql: &str) -> Vec<String> {
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

/// The shape this was found through.
#[test]
fn round528_group_by_alias_of_an_expression() {
    let mut e = engine();
    assert_eq!(
        rows(
            &mut e,
            "SELECT date_trunc('day', ts) AS d, count(*) FROM ga GROUP BY d ORDER BY d"
        ),
        vec!["2020-01-01 00:00:00|2", "2020-01-02 00:00:00|1"]
    );
    assert_eq!(
        rows(&mut e, "SELECT v * 2 AS w FROM ga GROUP BY w ORDER BY w"),
        vec!["2", "4", "10"]
    );
}

/// An alias of a plain column, and one of a constant — the two the
/// ROLLUP probe tripped on.
#[test]
fn round528_group_by_alias_of_a_column_or_constant() {
    let mut e = engine();
    assert_eq!(
        rows(
            &mut e,
            "SELECT v AS w, count(*) FROM ga GROUP BY w ORDER BY w"
        ),
        vec!["1|1", "2|1", "5|1"]
    );
    assert_eq!(
        rows(&mut e, "SELECT 'k' AS a, count(*) FROM ga GROUP BY a"),
        vec!["k|3"]
    );
}

/// HAVING still applies over the grouping the alias named.
#[test]
fn round528_having_over_an_aliased_group() {
    let mut e = engine();
    assert_eq!(
        rows(
            &mut e,
            "SELECT v AS w, count(*) FROM ga GROUP BY w HAVING count(*) > 0 ORDER BY w"
        ),
        vec!["1|1", "2|1", "5|1"]
    );
}

/// An INPUT column of that name WINS — measured: on a table that has a
/// `ts` column, `SELECT v AS ts … GROUP BY ts` groups by the COLUMN, so
/// PG then rejects the ungrouped `v`.
///
/// v7.39 (round 620) — the wording is PG's now. This pin used to record that
/// both refused it and said different things; the second half of that is no
/// longer true, so it asserts the message instead of merely `is_err`.
#[test]
fn round528_an_input_column_outranks_the_alias() {
    let mut e = engine();
    let err = e
        .execute("SELECT v AS ts, count(*) FROM ga GROUP BY ts")
        .expect_err("grouping by the input column leaves v ungrouped");
    assert!(
        format!("{err}").contains(r#"column "ga.v" must appear in the GROUP BY clause"#),
        "message was {err}"
    );
    // And a name that is neither is still a missing column.
    let err = e
        .execute("SELECT v FROM ga GROUP BY nosuch")
        .expect_err("no such name");
    assert!(format!("{err}").contains("nosuch"), "message was {err}");
}

/// PG's wording for the one alias that cannot be grouped by.
#[test]
fn round528_aggregate_alias_is_refused_pgs_way() {
    let mut e = engine();
    let err = e
        .execute("SELECT count(*) AS c FROM ga GROUP BY c")
        .expect_err("aggregate in GROUP BY");
    assert!(
        format!("{err}").contains("aggregate functions are not allowed in GROUP BY"),
        "message was {err}"
    );
}

/// Grouping by a real column and by an ordinal — the two that already
/// worked — are unchanged.
#[test]
fn round528_column_and_ordinal_grouping_unchanged() {
    let mut e = engine();
    assert_eq!(
        rows(&mut e, "SELECT v FROM ga GROUP BY v ORDER BY v").len(),
        3
    );
    assert_eq!(
        rows(&mut e, "SELECT v AS w FROM ga GROUP BY 1 ORDER BY 1").len(),
        3
    );
}
