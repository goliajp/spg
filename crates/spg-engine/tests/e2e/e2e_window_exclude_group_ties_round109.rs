//! v7.39 (read01 round 109) — window frame `EXCLUDE GROUP` / `EXCLUDE TIES`.
//!
//! `EXCLUDE GROUP` drops the current row AND its peers (same ORDER BY key) from
//! the frame; `EXCLUDE TIES` drops the peers but keeps the current row. SPG
//! parsed neither cleanly (`GROUP` is a reserved token, so the parser gave a
//! self-contradictory "expected … GROUP …, got Group") and the executor
//! rejected both as unsupported; value functions ignored EXCLUDE entirely.
//! Both frame loops now honour all four modes via the shared peer-group helper.
//! Locked byte-identical against live PG 18.4.

use spg_engine::{Engine, QueryResult};

fn rows(e: &mut Engine, sql: &str) -> Vec<String> {
    match e.execute(sql).unwrap_or_else(|x| panic!("{sql}: {x:?}")) {
        QueryResult::Rows { rows, .. } => rows
            .iter()
            .map(|r| {
                r.values
                    .iter()
                    .map(|v| match v {
                        spg_storage::Value::Null => "NULL".to_string(),
                        _ => spg_engine::eval::value_to_text(v),
                    })
                    .collect::<Vec<_>>()
                    .join("|")
            })
            .collect(),
        other => panic!("{sql}: {other:?}"),
    }
}

#[test]
fn exclude_group_and_ties_on_aggregate() {
    let mut e = Engine::new();
    assert_eq!(
        rows(
            &mut e,
            "SELECT x, sum(x) OVER (ORDER BY x GROUPS BETWEEN 1 PRECEDING AND 1 FOLLOWING EXCLUDE GROUP) FROM (VALUES(1),(1),(2),(3)) t(x)"
        ),
        ["1|2", "1|2", "2|5", "3|2"]
    );
    assert_eq!(
        rows(
            &mut e,
            "SELECT x, sum(x) OVER (ORDER BY x GROUPS BETWEEN 1 PRECEDING AND 1 FOLLOWING EXCLUDE TIES) FROM (VALUES(1),(1),(2),(3)) t(x)"
        ),
        ["1|3", "1|3", "2|7", "3|5"]
    );
}

#[test]
fn exclude_group_and_ties_on_value_functions() {
    let mut e = Engine::new();
    assert_eq!(
        rows(
            &mut e,
            "SELECT x, first_value(x) OVER (ORDER BY x ROWS BETWEEN UNBOUNDED PRECEDING AND UNBOUNDED FOLLOWING EXCLUDE TIES) FROM (VALUES(1),(1),(2)) t(x)"
        ),
        ["1|1", "1|1", "2|1"]
    );
    assert_eq!(
        rows(
            &mut e,
            "SELECT x, last_value(x) OVER (ORDER BY x GROUPS BETWEEN UNBOUNDED PRECEDING AND UNBOUNDED FOLLOWING EXCLUDE GROUP) FROM (VALUES(1),(1),(2)) t(x)"
        ),
        ["1|2", "1|2", "2|1"]
    );
}

#[test]
fn current_row_and_no_others_unchanged() {
    let mut e = Engine::new();
    assert_eq!(
        rows(
            &mut e,
            "SELECT x, count(*) OVER (ORDER BY x ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW EXCLUDE CURRENT ROW) FROM (VALUES(1),(2),(3)) t(x)"
        ),
        ["1|0", "2|1", "3|2"]
    );
    assert_eq!(
        rows(
            &mut e,
            "SELECT x, first_value(x) OVER (ORDER BY x ROWS BETWEEN UNBOUNDED PRECEDING AND UNBOUNDED FOLLOWING) FROM (VALUES(5),(6)) t(x)"
        ),
        ["5|5", "6|5"]
    );
}
