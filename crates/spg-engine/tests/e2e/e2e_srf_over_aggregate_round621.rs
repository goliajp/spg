//! v7.39 (round 621) — a set-returning item beside an aggregate was refused.
//!
//! `SELECT unnest(ARRAY[1,2]), count(*) FROM t` answered `function
//! unnest(integer[]) does not exist`. PG answers two rows, both carrying the
//! same count. The aggregate's projection evaluates each item scalarly against
//! the one synthetic row a group produces, so a call that returns a set had
//! nowhere to put its rows and reached the evaluator as an ordinary function.
//!
//! The shape that matters most is `unnest(array_agg(x))`, where the SRF's
//! ARGUMENT is the aggregate — the standard way to turn a group back into
//! rows. It failed too, and for the same reason.
//!
//! The aggregate's own output row is what the SRF expands over, so the
//! expansion belongs in that projection: each set-returning item is collected
//! as its whole list, and the group emits one row per element with the scalar
//! items repeated. Several of them expand in LOCKSTEP with the shorter padded
//! to NULL, which is round 67's rule for every other path — this is the last
//! of them to learn it.
//!
//! The items are recognised AFTER the aggregate rewrite, which is what lets
//! `unnest(array_agg(x))` be seen as an SRF over a synthetic column.
//!
//! Measured and NOT closed: only the builtin SRFs are recognised here. A user
//! `RETURNS SETOF` function inside an aggregate query keeps the old error,
//! because running its body needs the executor and the aggregate projection is
//! not it.
//!
//! All ten shapes were checked against live PG18 and match byte for byte.

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
    e.execute("CREATE TABLE ag (x INT, g INT)").unwrap();
    e.execute("INSERT INTO ag VALUES (3,1),(4,1),(5,2)")
        .unwrap();
    e
}

/// A constant SRF beside an aggregate, in both orders.
#[test]
fn round621_srf_beside_an_aggregate() {
    let mut e = seed();
    assert_eq!(
        vals(&mut e, "SELECT unnest(ARRAY[1,2]), count(*) FROM ag"),
        vec!["1|3", "2|3"],
        "two rows, both carrying the same count"
    );
    assert_eq!(
        vals(&mut e, "SELECT count(*), unnest(ARRAY[1,2]) FROM ag"),
        vec!["3|1", "3|2"],
        "and the other way round"
    );
    assert_eq!(
        vals(&mut e, "SELECT unnest(ARRAY[1,2]), sum(x) FROM ag"),
        vec!["1|12", "2|12"]
    );
    assert_eq!(
        vals(&mut e, "SELECT generate_series(1,2), count(*) FROM ag"),
        vec!["1|3", "2|3"],
        "the other builtin SRF"
    );
}

/// The SRF whose argument IS the aggregate — the reason this shape is written.
#[test]
fn round621_unnest_of_an_aggregate() {
    let mut e = seed();
    assert_eq!(
        vals(&mut e, "SELECT unnest(array_agg(x)) FROM ag ORDER BY 1"),
        vec!["3", "4", "5"],
        "a group turned back into rows"
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT g, unnest(array_agg(x)) FROM ag GROUP BY g ORDER BY 1,2"
        ),
        vec!["1|3", "1|4", "2|5"],
        "per group, and the group key repeats across its own expansion"
    );
}

/// With the clauses that surround an aggregate query.
#[test]
fn round621_with_group_by_having_and_order_by() {
    let mut e = seed();
    assert_eq!(
        vals(
            &mut e,
            "SELECT g, unnest(ARRAY[1,2]), count(*) FROM ag GROUP BY g ORDER BY 1,2"
        ),
        vec!["1|1|2", "1|2|2", "2|1|1", "2|2|1"],
        "every group expands, and the aggregate is per group"
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT unnest(ARRAY[1,2]), count(*) FROM ag ORDER BY 1"
        ),
        vec!["1|3", "2|3"]
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT unnest(ARRAY[1,2]), count(*) FROM ag HAVING count(*) > 0"
        ),
        vec!["1|3", "2|3"],
        "HAVING filters the GROUP, before the expansion"
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT unnest(ARRAY[1,2]), count(*) FROM ag HAVING count(*) > 99"
        ),
        Vec::<String>::new(),
        "and a group it removes expands to nothing at all"
    );
}

/// What must not have moved.
#[test]
fn round621_the_non_srf_aggregate_is_untouched() {
    let mut e = seed();
    assert_eq!(
        vals(&mut e, "SELECT count(*), sum(x) FROM ag"),
        vec!["3|12"]
    );
    assert_eq!(
        vals(&mut e, "SELECT g, count(*) FROM ag GROUP BY g ORDER BY 1"),
        vec!["1|2", "2|1"]
    );
    assert_eq!(
        vals(&mut e, "SELECT unnest(ARRAY[1,2]) FROM ag"),
        vec!["1", "2", "1", "2", "1", "2"],
        "an SRF with NO aggregate beside it takes the scan path, per input row"
    );
}
