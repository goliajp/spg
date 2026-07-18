//! v7.39 (round 230) — every aggregate is usable as a window function.
//! Before this round SPG carried a hardcoded list of 15 window functions
//! and answered anything else with "window function \"string_agg\" not
//! supported"; PG allows *any* aggregate after OVER. The generic path
//! drives the aggregate module's own accumulator over each row's frame,
//! so results and result types are whatever that aggregate produces in a
//! GROUP BY. Every expectation below was diffed against live PG18.4
//! (2026-07-19) over the same seven-row table.

use spg_engine::{Engine, QueryResult};

fn seeded() -> Engine {
    let mut e = Engine::new();
    e.execute("CREATE TABLE w (id int, g text, v int)").unwrap();
    e.execute("INSERT INTO w VALUES (1,'a',10),(2,'a',20),(3,'a',20),(4,'b',5),(5,'b',15),(6,'b',15),(7,'b',30)")
        .unwrap();
    e
}

/// Last column of each row, rendered the way `psql -tA` would.
fn col(e: &mut Engine, sql: &str) -> Vec<String> {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => rows
            .iter()
            .map(|r| match &r.values[r.values.len() - 1] {
                spg_storage::Value::Null => String::new(),
                spg_storage::Value::Text(s) => s.to_string(),
                spg_storage::Value::Bool(b) => (if *b { "t" } else { "f" }).to_string(),
                spg_storage::Value::Int(n) => n.to_string(),
                spg_storage::Value::BigInt(n) => n.to_string(),
                other => format!("{other:?}"),
            })
            .collect(),
        other => panic!("{other:?}"),
    }
}

fn err(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql) {
        Err(err) => format!("{err}"),
        Ok(ok) => panic!("expected an error, got {ok:?}"),
    }
}

#[test]
fn collection_aggregates_run_as_windows() {
    let mut e = seeded();
    assert_eq!(
        col(&mut e, "SELECT id, string_agg(g,',') OVER (PARTITION BY g ORDER BY v) FROM w ORDER BY id"),
        ["a", "a,a,a", "a,a,a", "b", "b,b,b", "b,b,b", "b,b,b,b"]
    );
    assert_eq!(
        col(&mut e, "SELECT id, array_agg(v) OVER (PARTITION BY g ORDER BY v)::text FROM w ORDER BY id"),
        ["{10}", "{10,20,20}", "{10,20,20}", "{5}", "{5,15,15}", "{5,15,15}", "{5,15,15,30}"]
    );
    assert_eq!(
        col(&mut e, "SELECT id, json_agg(v) OVER (PARTITION BY g ORDER BY v)::text FROM w ORDER BY id"),
        ["[10]", "[10, 20, 20]", "[10, 20, 20]", "[5]", "[5, 15, 15]", "[5, 15, 15]", "[5, 15, 15, 30]"]
    );
}

#[test]
fn boolean_bit_and_stat_aggregates_run_as_windows() {
    let mut e = seeded();
    assert_eq!(
        col(&mut e, "SELECT id, bool_or(v>10) OVER (PARTITION BY g)::text FROM w ORDER BY id"),
        ["true", "true", "true", "true", "true", "true", "true"]
    );
    assert_eq!(
        col(&mut e, "SELECT id, bool_and(v>10) OVER (PARTITION BY g)::text FROM w ORDER BY id"),
        ["false", "false", "false", "false", "false", "false", "false"]
    );
    // stddev/variance keep PG's exact NUMERIC result, not an f64.
    assert_eq!(
        col(&mut e, "SELECT id, stddev(v) OVER (PARTITION BY g)::text FROM w ORDER BY id"),
        [
            "5.7735026918962576",
            "5.7735026918962576",
            "5.7735026918962576",
            "10.3077640640441514",
            "10.3077640640441514",
            "10.3077640640441514",
            "10.3077640640441514",
        ]
    );
    assert_eq!(
        col(&mut e, "SELECT id, bit_or(v) OVER (PARTITION BY g)::text FROM w ORDER BY id"),
        ["30", "30", "30", "31", "31", "31", "31"]
    );
    assert_eq!(
        col(&mut e, "SELECT id, any_value(v) OVER (PARTITION BY g)::text FROM w ORDER BY id"),
        ["10", "10", "10", "5", "5", "5", "5"]
    );
}

#[test]
fn generic_aggregates_honour_frame_exclude_and_filter() {
    let mut e = seeded();
    // A moving frame, not the whole partition.
    assert_eq!(
        col(
            &mut e,
            "SELECT id, string_agg(g,'-') OVER (PARTITION BY g ORDER BY v \
             ROWS BETWEEN 1 PRECEDING AND CURRENT ROW) FROM w ORDER BY id"
        ),
        ["a", "a-a", "a-a", "b", "b-b", "b-b", "b-b"]
    );
    // EXCLUDE CURRENT ROW drops the row itself from its own frame.
    assert_eq!(
        col(
            &mut e,
            "SELECT id, array_agg(v) OVER (PARTITION BY g ORDER BY v \
             ROWS BETWEEN UNBOUNDED PRECEDING AND UNBOUNDED FOLLOWING EXCLUDE CURRENT ROW)::text \
             FROM w ORDER BY id"
        ),
        ["{20,20}", "{10,20}", "{10,20}", "{15,15,30}", "{5,15,30}", "{5,15,30}", "{5,15,15}"]
    );
    // FILTER restricts which frame rows contribute.
    assert_eq!(
        col(
            &mut e,
            "SELECT id, array_agg(v) FILTER (WHERE v > 10) OVER (PARTITION BY g)::text \
             FROM w ORDER BY id"
        ),
        ["{20,20}", "{20,20}", "{20,20}", "{15,15,30}", "{15,15,30}", "{15,15,30}", "{15,15,30}"]
    );
}

#[test]
fn distinct_and_aggregate_order_by_are_refused_not_ignored() {
    let mut e = seeded();
    // Both modifiers used to be parsed and silently dropped, so
    // `count(DISTINCT v) OVER (…)` answered the plain count (3 / 4 here,
    // where the distinct counts are 2 / 3). PG implements neither.
    let got = err(&mut e, "SELECT count(DISTINCT v) OVER (PARTITION BY g) FROM w");
    assert!(got.contains("DISTINCT is not implemented for window functions"), "{got}");
    let got = err(&mut e, "SELECT array_agg(v ORDER BY v DESC) OVER (PARTITION BY g) FROM w");
    assert!(
        got.contains("aggregate ORDER BY is not implemented for window functions"),
        "{got}"
    );
}

#[test]
fn a_non_aggregate_after_over_reports_a_missing_function() {
    let mut e = seeded();
    let got = err(&mut e, "SELECT nosuchfn(v) OVER (PARTITION BY g) FROM w");
    assert!(got.contains("function nosuchfn() does not exist"), "{got}");
    // The old wording leaked SPG's internal whitelist at the user.
    assert!(!got.contains("v4.21"), "no internal list: {got}");
}
