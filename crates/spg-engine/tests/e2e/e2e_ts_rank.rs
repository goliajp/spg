//! v7.38 (read01, T12.1) — ts_rank exact PG formulas: calc_rank_or (per-term
//! positional sum / distinct-term count) and calc_rank_and (probabilistic-OR of
//! distance-weighted position pairs) with the default normalization (flag 0).
//! Oracle: live PG 18.4 (values matched to ~7 significant figures).

use spg_engine::{Engine, QueryResult};

fn near(e: &mut Engine, sql: &str, expected: f32) {
    let v = match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => match rows[0].values[0] {
            spg_storage::Value::Float(x) => x as f32,
            spg_storage::Value::Real(x) => x,
            ref o => panic!("not a float: {o:?}"),
        },
        _ => panic!("rows"),
    };
    assert!((v - expected).abs() < 1e-5, "got {v}, expected {expected}");
}

#[test]
fn ts_rank_matches_pg() {
    let mut e = Engine::new();
    // calc_rank_or.
    near(&mut e, "SELECT ts_rank('cat:1,2,3'::tsvector, 'cat'::tsquery)", 0.082_745_634);
    near(&mut e, "SELECT ts_rank('cat:1'::tsvector, 'cat'::tsquery)", 0.060_792_71);
    // calc_rank_and (distance-weighted).
    near(&mut e, "SELECT ts_rank('cat:1 rat:2'::tsvector, 'cat & rat'::tsquery)", 0.099_103_22);
    near(&mut e, "SELECT ts_rank('cat:1 rat:2 bat:3'::tsvector, 'cat & rat & bat'::tsquery)", 0.268_329_77);
    near(&mut e, "SELECT ts_rank('cat:1A rat:2A'::tsvector, 'cat & rat'::tsquery)", 0.991_032_2);
    near(&mut e, "SELECT ts_rank('cat:1 rat:10'::tsvector, 'cat & rat'::tsquery)", 0.051_744_01);
    // No match → 0.
    near(&mut e, "SELECT ts_rank('cat:1'::tsvector, 'dog'::tsquery)", 0.0);
}
