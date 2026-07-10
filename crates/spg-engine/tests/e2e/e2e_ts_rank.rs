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
    near(
        &mut e,
        "SELECT ts_rank('cat:1,2,3'::tsvector, 'cat'::tsquery)",
        0.082_745_634,
    );
    near(
        &mut e,
        "SELECT ts_rank('cat:1'::tsvector, 'cat'::tsquery)",
        0.060_792_71,
    );
    // calc_rank_and (distance-weighted).
    near(
        &mut e,
        "SELECT ts_rank('cat:1 rat:2'::tsvector, 'cat & rat'::tsquery)",
        0.099_103_22,
    );
    near(
        &mut e,
        "SELECT ts_rank('cat:1 rat:2 bat:3'::tsvector, 'cat & rat & bat'::tsquery)",
        0.268_329_77,
    );
    near(
        &mut e,
        "SELECT ts_rank('cat:1A rat:2A'::tsvector, 'cat & rat'::tsquery)",
        0.991_032_2,
    );
    near(
        &mut e,
        "SELECT ts_rank('cat:1 rat:10'::tsvector, 'cat & rat'::tsquery)",
        0.051_744_01,
    );
    // No match → 0.
    near(
        &mut e,
        "SELECT ts_rank('cat:1'::tsvector, 'dog'::tsquery)",
        0.0,
    );

    // ts_rank_cd cover density: (#entries / Σ 1/weight) / (noise + 1) per cover.
    near(
        &mut e,
        "SELECT ts_rank_cd('cat:1 rat:2'::tsvector, 'cat & rat'::tsquery)",
        0.1,
    );
    near(
        &mut e,
        "SELECT ts_rank_cd('cat:1 rat:3'::tsvector, 'cat & rat'::tsquery)",
        0.05,
    );
    near(
        &mut e,
        "SELECT ts_rank_cd('cat:1 rat:10'::tsvector, 'cat & rat'::tsquery)",
        0.011_111_111,
    );
    // Two covers: 0.1 + 0.1/3.
    near(
        &mut e,
        "SELECT ts_rank_cd('cat:1,5 rat:2'::tsvector, 'cat & rat'::tsquery)",
        0.133_333_34,
    );
    near(
        &mut e,
        "SELECT ts_rank_cd('cat:1'::tsvector, 'dog'::tsquery)",
        0.0,
    );

    // Custom weight array [D, C, B, A] (float8[]); B-weight drives a B/B pair.
    near(
        &mut e,
        "SELECT ts_rank(ARRAY[0.5,0.6,0.7,0.8]::float8[], 'cat:1B rat:2B'::tsvector, 'cat & rat'::tsquery)",
        0.693_722_5,
    );
    near(
        &mut e,
        "SELECT ts_rank_cd(ARRAY[0.5,0.6,0.7,0.8]::float8[], 'cat:1B rat:2B'::tsvector, 'cat & rat'::tsquery)",
        0.7,
    );
    // Explicit norm 0 is accepted; a non-zero flag errors honestly.
    near(
        &mut e,
        "SELECT ts_rank('cat:1,2,3'::tsvector, 'cat'::tsquery, 0)",
        0.082_745_634,
    );
    // Normalization bitmask (verified vs PG): 1 → /log2(len+1), 2 → /len,
    // 8 → /uniq, 16 → /log2(uniq+1), 32 → r/(r+1); combos apply in sequence.
    near(
        &mut e,
        "SELECT ts_rank('a:1,2,3'::tsvector, 'a'::tsquery, 1)",
        0.041_372_817,
    );
    near(
        &mut e,
        "SELECT ts_rank('a:1,2,3'::tsvector, 'a'::tsquery, 2)",
        0.027_581_878,
    );
    near(
        &mut e,
        "SELECT ts_rank('a:1,2,3'::tsvector, 'a'::tsquery, 32)",
        0.076_422_04,
    );
    near(
        &mut e,
        "SELECT ts_rank('a:1 b:2 c:4'::tsvector, 'a & b & c'::tsquery, 8)",
        0.088_970_9,
    );
    near(
        &mut e,
        "SELECT ts_rank('a:1 b:2 c:4'::tsvector, 'a & b & c'::tsquery, 16)",
        0.133_456_38,
    );
    near(
        &mut e,
        "SELECT ts_rank('a:1 b:2 c:4'::tsvector, 'a & b & c'::tsquery, 3)",
        0.044_485_46,
    );
    // Cover-extent flag 4 is not yet supported for ts_rank_cd (honest error).
    assert!(
        e.execute("SELECT ts_rank_cd('a:1 b:2'::tsvector, 'a & b'::tsquery, 4)")
            .is_err()
    );
    // Unknown flag bits error.
    assert!(
        e.execute("SELECT ts_rank('a:1'::tsvector, 'a'::tsquery, 64)")
            .is_err()
    );
}
