//! v7.39.11 — an aggregate anywhere in the statement made `ORDER BY
//! <text>` sort by BYTES.
//!
//! Reported by sentori against the published 7.39.10 image and
//! reproduced here against PostgreSQL 18.6. Same column, same `ORDER
//! BY`, same rows; the only difference is whether an aggregate appears
//! somewhere else in the query:
//!
//! ```text
//!                                              PG 18    SPG 7.39.10
//!   GROUP BY t ORDER BY t                      a A b B    a A b B
//!   GROUP BY t ORDER BY t, t                   a A b B    a A b B
//!   GROUP BY t ORDER BY t   + count(*)         a A b B    A B a b
//!   GROUP BY t ORDER BY t, count(*)            a A b B    A B a b
//!   GROUP BY t HAVING count(*) > 0 ORDER BY t  a A b B    A B a b
//!   DISTINCT t, count(*) OVER () ORDER BY t    a A b B    A B a b
//!   GROUP BY t ORDER BY 1   + count(*)         a A b B    A B a b
//! ```
//!
//! No row is wrong and nothing raises — only the order changes. It is
//! the same class as the restart that changed a sort order in v7.39.5:
//! a report run one way and the same report run another way disagree,
//! with no error on either side.
//!
//! The comparator for a grouped sort resolved each key's collation from
//! the COLUMN's own declared name and nothing else. A plain `TEXT`
//! column declares none, so the comparator was handed `None` and fell
//! back to byte order, while the ungrouped path asks the database. It
//! arrived with the collation switch in v7.38.22 and survived every
//! release since, including v7.39.5, which was the collation release.
//!
//! One correction to the report this came from: it lists `ORDER BY 1`
//! with an aggregate as CORRECT on 7.39.10. Measured on the published
//! image it is not — `SELECT t, count(*) … ORDER BY 1` answers
//! `A B a b` there, the same as the named form. The positional and
//! named spellings reach the same sort; what decides it is the
//! aggregate alone, which makes the rule simpler than the report's
//! three-way narrowing.

use spg_engine::{Engine, QueryResult};

fn collated() -> Engine {
    let mut e = Engine::new();
    e.declare_database_collation("en_US.UTF-8")
        .expect("the test engine accepts a database collation");
    e.execute("CREATE TABLE agg_ord (t TEXT, n INT)").unwrap();
    e.execute("INSERT INTO agg_ord VALUES ('a',1),('A',1),('b',1),('B',1)")
        .unwrap();
    e
}

/// The first column of every row, joined — the order is the answer.
fn order(e: &mut Engine, sql: &str) -> String {
    let QueryResult::Rows { rows, .. } = e.execute(sql).unwrap_or_else(|x| panic!("{sql}: {x:?}"))
    else {
        panic!("{sql}: expected Rows");
    };
    rows.iter()
        .map(|r| match &r.values[0] {
            spg_storage::Value::Text(t) => t.to_string(),
            other => panic!("{sql}: {other:?}"),
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// What PostgreSQL 18.6 answers for every one of these, measured.
const COLLATED: &str = "a A b B";
/// What byte order gives, and what `COLLATE \"C\"` must still give.
const BYTES: &str = "A B a b";

#[test]
fn an_aggregate_in_the_select_list_does_not_change_the_order() {
    let mut e = collated();
    assert_eq!(
        order(
            &mut e,
            "SELECT t, count(*) FROM agg_ord GROUP BY t ORDER BY t"
        ),
        COLLATED
    );
}

#[test]
fn an_aggregate_in_the_order_by_does_not_change_the_order() {
    let mut e = collated();
    assert_eq!(
        order(
            &mut e,
            "SELECT t FROM agg_ord GROUP BY t ORDER BY t, count(*)"
        ),
        COLLATED
    );
}

#[test]
fn an_aggregate_in_having_alone_does_not_change_the_order() {
    // No aggregate in the select list at all — this is the case that
    // says the defect is not about GROUP BY.
    let mut e = collated();
    assert_eq!(
        order(
            &mut e,
            "SELECT t FROM agg_ord GROUP BY t HAVING count(*) > 0 ORDER BY t"
        ),
        COLLATED
    );
}

#[test]
fn a_window_aggregate_with_no_group_by_does_not_change_the_order() {
    let mut e = collated();
    assert_eq!(
        order(
            &mut e,
            "SELECT DISTINCT t, count(*) OVER () FROM agg_ord ORDER BY t"
        ),
        COLLATED
    );
}

#[test]
fn the_positional_spelling_answers_the_same_as_the_named_one() {
    // The report has this one as correct on 7.39.10; measured on the
    // published image it degrades exactly like `ORDER BY t`. Pinning
    // both spellings together is what keeps them from drifting apart
    // again.
    let mut e = collated();
    let named = order(
        &mut e,
        "SELECT t, count(*) FROM agg_ord GROUP BY t ORDER BY t",
    );
    let positional = order(
        &mut e,
        "SELECT t, count(*) FROM agg_ord GROUP BY t ORDER BY 1",
    );
    assert_eq!(named, COLLATED);
    assert_eq!(positional, COLLATED);
    assert_eq!(named, positional);
}

#[test]
fn the_shapes_without_an_aggregate_were_already_right() {
    let mut e = collated();
    assert_eq!(
        order(&mut e, "SELECT t FROM agg_ord GROUP BY t ORDER BY t"),
        COLLATED
    );
    assert_eq!(
        order(&mut e, "SELECT t FROM agg_ord GROUP BY t ORDER BY t, t"),
        COLLATED
    );
}

#[test]
fn collate_c_still_asks_for_bytes() {
    // The escape hatch has to keep working: it is what a caller writes
    // when it wants byte order, and PostgreSQL answers `A B a b` here.
    let mut e = collated();
    assert_eq!(
        order(
            &mut e,
            "SELECT t, count(*) FROM agg_ord GROUP BY t ORDER BY t COLLATE \"C\""
        ),
        BYTES
    );
}

#[test]
fn a_byte_ordering_database_is_unchanged() {
    // `C` resolves to no collation, so a database that never asked for
    // a locale takes the path it always did.
    let mut e = Engine::new();
    e.execute("CREATE TABLE agg_ord (t TEXT, n INT)").unwrap();
    e.execute("INSERT INTO agg_ord VALUES ('a',1),('A',1),('b',1),('B',1)")
        .unwrap();
    assert_eq!(
        order(
            &mut e,
            "SELECT t, count(*) FROM agg_ord GROUP BY t ORDER BY t"
        ),
        BYTES
    );
}
