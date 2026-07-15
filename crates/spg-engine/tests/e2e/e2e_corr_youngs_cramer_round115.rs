//! v7.39 (read01 round 115) — regression aggregates match PG's float8 output
//! to the last ULP (Youngs-Cramer accumulation).
//!
//! `corr(y, x)` returned `0.9819805060619656` where PG gives
//! `0.9819805060619659`. SPG accumulated the raw sums of squares and derived
//! Sxx/Syy/Sxy as `Σx² − (Σx)²/n` at finalize time; PG accumulates the sums of
//! squared deviations incrementally (Youngs-Cramer). The two are
//! mathematically equal but round differently in float8, so `corr` drifted in
//! the 16th digit. SPG now uses the same incremental update. Locked
//! byte-identical against PG 18.4.

use spg_engine::{Engine, QueryResult};

fn text(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap_or_else(|x| panic!("{sql}: {x:?}")) {
        QueryResult::Rows { rows, .. } => match &rows[0].values[0] {
            spg_storage::Value::Null => "NULL".to_string(),
            v => spg_engine::eval::value_to_text(v),
        },
        other => panic!("{sql}: {other:?}"),
    }
}

#[test]
fn corr_matches_pg_to_last_ulp() {
    let mut e = Engine::new();
    assert_eq!(
        text(&mut e, "SELECT corr(y, x)::text FROM (VALUES(1,1),(2,2),(3,4)) t(x,y)"),
        "0.9819805060619659"
    );
    // A messier, decimal dataset (naive accumulation drifted here too).
    assert_eq!(
        text(&mut e, "SELECT corr(y,x)::text FROM (VALUES(1,2.5),(2,3.7),(3,1.1),(4,9.9),(5,4.2)) t(x,y)"),
        "0.4515060623032481"
    );
    // Self-correlation is exactly 1 (regression guard).
    assert_eq!(
        text(&mut e, "SELECT corr(x, x)::text FROM (VALUES(1),(2),(3)) t(x)"),
        "1"
    );
}

#[test]
fn covar_and_regr_family_match_pg() {
    let mut e = Engine::new();
    let rows = "(VALUES(1,1),(2,3),(3,5),(4,6)) t(x,y)";
    assert_eq!(text(&mut e, &format!("SELECT covar_pop(y,x)::text FROM {rows}")), "2.125");
    assert_eq!(
        text(&mut e, &format!("SELECT regr_slope(y,x)::text FROM {rows}")),
        "1.7"
    );
    assert_eq!(
        text(&mut e, &format!("SELECT regr_intercept(y,x)::text FROM {rows}")),
        "-0.5"
    );
    assert_eq!(
        text(&mut e, &format!("SELECT regr_r2(y,x)::text FROM {rows}")),
        "0.9796610169491525"
    );
    assert_eq!(text(&mut e, &format!("SELECT regr_sxx(y,x)::text FROM {rows}")), "5");
    assert_eq!(text(&mut e, &format!("SELECT regr_syy(y,x)::text FROM {rows}")), "14.75");
    assert_eq!(text(&mut e, &format!("SELECT regr_sxy(y,x)::text FROM {rows}")), "8.5");
    assert_eq!(text(&mut e, &format!("SELECT regr_avgx(y,x)::text FROM {rows}")), "2.5");
    assert_eq!(text(&mut e, &format!("SELECT regr_avgy(y,x)::text FROM {rows}")), "3.75");
}
