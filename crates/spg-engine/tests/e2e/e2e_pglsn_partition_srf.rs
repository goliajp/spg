//! v7.39 (read01 utils/adt, round 22) — pg_lsn.c (the full pg_lsn type:
//! I/O, ordering, byte arithmetic, lsn difference) + partitionfuncs.c
//! (pg_partition_tree / pg_partition_ancestors in FROM position).
//! Byte-locked vs PG18.

use spg_engine::{Engine, QueryResult};

fn row_of(e: &mut Engine, sql: &str) -> Vec<String> {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => rows[0]
            .values
            .iter()
            .map(spg_engine::eval::value_to_text)
            .collect(),
        other => panic!("{sql}: {other:?}"),
    }
}

fn col_of(e: &mut Engine, sql: &str) -> Vec<String> {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => rows
            .iter()
            .map(|r| spg_engine::eval::value_to_text(&r.values[0]))
            .collect(),
        other => panic!("{sql}: {other:?}"),
    }
}

fn err_of(e: &mut Engine, sql: &str) -> String {
    format!("{}", e.execute(sql).unwrap_err())
}

#[test]
fn pg_lsn_io_ordering_arithmetic() {
    let mut e = Engine::new();
    assert_eq!(
        row_of(
            &mut e,
            "SELECT '0/16B3748'::pg_lsn, 'FFFFFFFF/FFFFFFFF'::pg_lsn, pg_lsn '0/0'"
        ),
        vec!["0/16B3748", "FFFFFFFF/FFFFFFFF", "0/0"]
    );
    // Ordering is by the 64-bit location, not text.
    assert_eq!(
        row_of(
            &mut e,
            "SELECT '0/16B3748'::pg_lsn < '0/16B3749'::pg_lsn, \
             '1/0'::pg_lsn > '0/FFFFFFFF'::pg_lsn"
        ),
        vec!["true", "true"]
    );
    assert_eq!(
        row_of(
            &mut e,
            "SELECT '0/16B3748'::pg_lsn + 16, '0/16B3748'::pg_lsn - 8, \
             '0/16B3749'::pg_lsn - '0/16B3748'::pg_lsn"
        ),
        vec!["0/16B3758", "0/16B3740", "1"]
    );
    assert_eq!(
        row_of(
            &mut e,
            "SELECT pg_typeof('1/0'::pg_lsn - '0/0'::pg_lsn), pg_typeof('0/1'::pg_lsn)"
        ),
        vec!["numeric", "pg_lsn"]
    );
    assert_eq!(
        row_of(
            &mut e,
            "SELECT max(x) FROM (VALUES ('0/1'::pg_lsn),('1/0'::pg_lsn)) t(x)"
        ),
        vec!["1/0"]
    );
    assert!(err_of(&mut e, "SELECT 'x/0'::pg_lsn")
        .contains("invalid input syntax for type pg_lsn: \"x/0\""));
    assert!(err_of(&mut e, "SELECT '0/16B3748'::pg_lsn - 100000000000")
        .contains("pg_lsn out of range"));
}

#[test]
fn partition_tree_and_ancestors_srf() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE pt (id int, r int) PARTITION BY RANGE (r)")
        .unwrap();
    e.execute("CREATE TABLE pt_a PARTITION OF pt FOR VALUES FROM (0) TO (10)")
        .unwrap();
    e.execute("CREATE TABLE pt_b PARTITION OF pt FOR VALUES FROM (10) TO (20)")
        .unwrap();
    e.execute("CREATE TABLE plain_t (id int)").unwrap();
    assert_eq!(
        col_of(
            &mut e,
            "SELECT relid || '|' || coalesce(parentrelid, '') || '|' || isleaf::text \
             || '|' || level FROM pg_partition_tree('pt')"
        ),
        vec!["pt||false|0", "pt_a|pt|true|1", "pt_b|pt|true|1"]
    );
    // Mid-tree start: parentrelid stays the REAL parent.
    assert_eq!(
        col_of(
            &mut e,
            "SELECT relid || '|' || parentrelid || '|' || level \
             FROM pg_partition_tree('pt_b')"
        ),
        vec!["pt_b|pt|0"]
    );
    assert_eq!(
        col_of(&mut e, "SELECT relid FROM pg_partition_ancestors('pt_b')"),
        vec!["pt_b", "pt"]
    );
    // Column aliasing + WHERE over the SRF columns.
    assert_eq!(
        col_of(
            &mut e,
            "SELECT relid FROM pg_partition_tree('pt') AS t(relid, p, l, lvl) \
             WHERE lvl > 0 ORDER BY relid"
        ),
        vec!["pt_a", "pt_b"]
    );
    // A relation outside any partition tree yields no rows; a missing
    // one errors.
    assert_eq!(
        col_of(&mut e, "SELECT relid FROM pg_partition_tree('plain_t')"),
        Vec::<String>::new()
    );
    assert_eq!(
        col_of(&mut e, "SELECT relid FROM pg_partition_ancestors('plain_t')"),
        Vec::<String>::new()
    );
    assert!(err_of(&mut e, "SELECT relid FROM pg_partition_tree('nope')")
        .contains("relation \"nope\" does not exist"));
}
