//! 9.0.0 — `FILTER` leaked onto a LATER aggregate.
//!
//! Measured against PostgreSQL 18.6 on five rows:
//!
//! | query | SPG | PG |
//! |---|---|---|
//! | `count(*) FILTER (WHERE g='a'), count(*)` | `2\|2` | `2\|5` |
//! | `sum(v) FILTER (WHERE g='a'), sum(v)` | `30\|30` | `30\|150` |
//! | `max(v) FILTER (WHERE g='a'), max(v)` | `20\|20` | `20\|50` |
//! | `… GROUP BY g` (g='a' row) | `0\|0` | `0\|2` |
//!
//! The unfiltered aggregate was matched to the earlier FILTERed spec by
//! name and arguments, so it answered the filtered value. It is correct
//! when the unfiltered one is written FIRST — which is why a decade of
//! `SELECT count(*), count(*) FILTER (…)` never caught it.
//!
//! `SELECT count(*) FILTER (WHERE …), count(*)` is the ordinary
//! reporting idiom: a total beside a subtotal. It answered the subtotal
//! twice, silently.

use spg_engine::{Engine, QueryResult};

fn row(e: &mut Engine, sql: &str) -> Vec<String> {
    let QueryResult::Rows { rows, .. } = e
        .execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"))
    else {
        panic!("expected rows for {sql}");
    };
    rows.into_iter()
        .flat_map(|r| {
            r.values
                .iter()
                .map(spg_engine::eval::value_to_text)
                .collect::<Vec<_>>()
        })
        .collect()
}

fn seeded() -> Engine {
    let mut e = Engine::new();
    for sql in [
        "CREATE TABLE ft (id int, g text, v int)",
        "INSERT INTO ft VALUES (1,'a',10),(2,'a',20),(3,'b',30),(4,'b',40),(5,'b',50)",
    ] {
        e.execute(sql)
            .unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    }
    e
}

#[test]
fn an_unfiltered_aggregate_after_a_filtered_one_counts_every_row() {
    let mut e = seeded();
    assert_eq!(
        row(
            &mut e,
            "SELECT count(*) FILTER (WHERE g='a'), count(*) FROM ft"
        ),
        vec!["2".to_string(), "5".to_string()]
    );
    assert_eq!(
        row(&mut e, "SELECT sum(v) FILTER (WHERE g='a'), sum(v) FROM ft"),
        vec!["30".to_string(), "150".to_string()]
    );
    assert_eq!(
        row(&mut e, "SELECT max(v) FILTER (WHERE g='a'), max(v) FROM ft"),
        vec!["20".to_string(), "50".to_string()]
    );
}

#[test]
fn two_filters_then_an_unfiltered_one() {
    // The unfiltered one used to take the FIRST spec's filter, so this
    // answered 2 where PostgreSQL 18.6 answers 5.
    let mut e = seeded();
    assert_eq!(
        row(
            &mut e,
            "SELECT count(*) FILTER (WHERE g='a'), count(*) FILTER (WHERE g='b'), count(*) FROM ft"
        ),
        vec!["2".to_string(), "3".to_string(), "5".to_string()]
    );
}

#[test]
fn the_same_leak_under_group_by() {
    let mut e = seeded();
    assert_eq!(
        row(
            &mut e,
            "SELECT g, count(*) FILTER (WHERE v>20), count(*) FROM ft GROUP BY g ORDER BY g"
        ),
        vec![
            "a".to_string(),
            "0".to_string(),
            "2".to_string(),
            "b".to_string(),
            "3".to_string(),
            "3".to_string(),
        ]
    );
}

#[test]
fn the_order_that_always_worked_still_works() {
    // The control: written the other way round it was correct before the
    // fix, so it must stay correct after it.
    let mut e = seeded();
    assert_eq!(
        row(
            &mut e,
            "SELECT count(*), count(*) FILTER (WHERE g='a') FROM ft"
        ),
        vec!["5".to_string(), "2".to_string()]
    );
    // Two aggregates that differ only in their FILTER keep their own
    // answers — the dedup at collection time already compared them.
    assert_eq!(
        row(
            &mut e,
            "SELECT count(*) FILTER (WHERE g='a'), count(*) FILTER (WHERE g='b') FROM ft"
        ),
        vec!["2".to_string(), "3".to_string()]
    );
}
