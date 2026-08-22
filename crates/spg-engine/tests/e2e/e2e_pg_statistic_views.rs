//! v7.37.23 (23.7-a) + v7.37.24 (24.15) — pg_statistic_ext +
//! pg_statistic views.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

#[test]
fn pg_statistic_ext_returns_empty_with_pg_columns() {
    let mut e = Engine::new();
    let r = e
        .execute("SELECT * FROM pg_catalog.pg_statistic_ext")
        .unwrap();
    let QueryResult::Rows { columns, rows } = r else {
        panic!("Rows");
    };
    let names: Vec<&str> = columns.iter().map(|c| c.name.as_str()).collect();
    for must in [
        "oid",
        "stxrelid",
        "stxname",
        "stxnamespace",
        "stxowner",
        "stxkind",
        "stxkeys",
    ] {
        assert!(
            names.contains(&must),
            "pg_statistic_ext missing {must}: {names:?}"
        );
    }
    assert!(rows.is_empty());
}

#[test]
fn pg_statistic_lists_one_row_per_column() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (a INT, b TEXT, c BIGINT)")
        .unwrap();
    // v7.38.18 — a column ANALYZE has not visited has NO row, as in
    // PostgreSQL 18.4 (measured on this exact table: 0 before, 3
    // after).
    //
    // This test used to assert three rows straight after CREATE, which
    // pinned a stub: `pg_statistic` emitted one all-zero row per
    // column whether or not anything had been analysed. A test could
    // then count those rows and prove nothing — and one did, an hour
    // before this was found: the S10 pin read them as evidence that
    // ANALYZE had run.
    let before = e.execute("SELECT * FROM pg_catalog.pg_statistic").unwrap();
    let QueryResult::Rows {
        rows: before_rows, ..
    } = before
    else {
        panic!("Rows");
    };
    assert!(
        before_rows.is_empty(),
        "un-analysed columns must not appear in pg_statistic, got {}",
        before_rows.len()
    );
    e.execute("ANALYZE t").unwrap();
    let r = e.execute("SELECT * FROM pg_catalog.pg_statistic").unwrap();
    let QueryResult::Rows { columns, rows } = r else {
        panic!("Rows");
    };
    let names: Vec<&str> = columns.iter().map(|c| c.name.as_str()).collect();
    for must in [
        "starelid",
        "staattnum",
        "stainherit",
        "stanullfrac",
        "stawidth",
        "stadistinct",
    ] {
        assert!(
            names.contains(&must),
            "pg_statistic missing {must}: {names:?}"
        );
    }
    // 3 columns analysed → 3 rows.
    assert_eq!(rows.len(), 3);
    // staattnum at position 1 should be 1, 2, 3.
    let mut attnums: Vec<i16> = rows
        .iter()
        .filter_map(|r| {
            if let Value::SmallInt(n) = r.values[1] {
                Some(n)
            } else {
                None
            }
        })
        .collect();
    attnums.sort();
    assert_eq!(attnums, vec![1, 2, 3]);
}

#[test]
fn pg_statistic_empty_when_no_user_tables() {
    let mut e = Engine::new();
    let r = e.execute("SELECT * FROM pg_catalog.pg_statistic").unwrap();
    let QueryResult::Rows { rows, .. } = r else {
        panic!("Rows");
    };
    assert!(rows.is_empty());
}
