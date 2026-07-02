//! v7.37.17 (17.6 siblings) — GROUP BY ROLLUP(a, b): subtotal
//! rows via UNION ALL expansion.

use spg_engine::{Engine, QueryResult};

fn rows(e: &mut Engine, sql: &str) -> Vec<Vec<spg_storage::Value<'static>>> {
    let r = e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    let QueryResult::Rows { rows, .. } = r else {
        panic!("expected Rows");
    };
    rows.into_iter()
        .map(|row| row.values.into_iter().collect())
        .collect()
}

fn as_i64(v: &spg_storage::Value<'_>) -> i64 {
    match v {
        spg_storage::Value::Int(n) => i64::from(*n),
        spg_storage::Value::BigInt(n) => *n,
        other => panic!("expected integer, got {other:?}"),
    }
}

fn cell(v: &spg_storage::Value<'_>) -> Option<String> {
    match v {
        spg_storage::Value::Text(s) => Some(s.to_string()),
        spg_storage::Value::Null => None,
        other => panic!("expected Text/Null, got {other:?}"),
    }
}

#[test]
fn two_key_rollup_subtotals() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE sales (region TEXT, product TEXT, amt INT)")
        .unwrap();
    e.execute(
        "INSERT INTO sales VALUES \
         ('east', 'a', 10), ('east', 'b', 20), ('west', 'a', 5)",
    )
    .unwrap();
    let got = rows(
        &mut e,
        "SELECT region, product, SUM(amt) FROM sales \
         GROUP BY ROLLUP(region, product) \
         ORDER BY 1 NULLS LAST, 2 NULLS LAST",
    );
    // 3 detail rows + 2 region subtotals + 1 grand total.
    assert_eq!(got.len(), 6);
    // Detail: (east, a, 10).
    assert_eq!(cell(&got[0][0]).as_deref(), Some("east"));
    assert_eq!(cell(&got[0][1]).as_deref(), Some("a"));
    assert_eq!(as_i64(&got[0][2]), 10);
    // Region subtotal: (east, NULL, 30).
    assert_eq!(cell(&got[2][0]).as_deref(), Some("east"));
    assert_eq!(cell(&got[2][1]), None);
    assert_eq!(as_i64(&got[2][2]), 30);
    // Grand total: (NULL, NULL, 35).
    assert_eq!(cell(&got[5][0]), None);
    assert_eq!(cell(&got[5][1]), None);
    assert_eq!(as_i64(&got[5][2]), 35);
}

#[test]
fn single_key_rollup() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t1 (k TEXT, v INT)").unwrap();
    e.execute("INSERT INTO t1 VALUES ('x', 1), ('x', 2), ('y', 3)")
        .unwrap();
    let got = rows(
        &mut e,
        "SELECT k, SUM(v) FROM t1 GROUP BY ROLLUP(k) ORDER BY 1 NULLS LAST",
    );
    assert_eq!(got.len(), 3);
    assert_eq!(as_i64(&got[0][1]), 3); // x
    assert_eq!(as_i64(&got[1][1]), 3); // y
    assert_eq!(cell(&got[2][0]), None); // grand total
    assert_eq!(as_i64(&got[2][1]), 6);
}

#[test]
fn cube_all_subsets() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE c2 (a TEXT, b TEXT, v INT)").unwrap();
    e.execute("INSERT INTO c2 VALUES ('x', 'p', 1), ('x', 'q', 2), ('y', 'p', 4)")
        .unwrap();
    let got = rows(
        &mut e,
        "SELECT a, b, SUM(v) FROM c2 GROUP BY CUBE(a, b) \
         ORDER BY 1 NULLS LAST, 2 NULLS LAST",
    );
    // 3 detail + 2 per-a + 2 per-b + 1 grand = 8.
    assert_eq!(got.len(), 8);
    // Per-b subtotal (NULL, 'p', 5) appears — the grouping ROLLUP
    // can't produce.
    assert!(got.iter().any(|r| {
        cell(&r[0]).is_none()
            && cell(&r[1]).as_deref() == Some("p")
            && as_i64(&r[2]) == 5
    }));
    // Grand total (NULL, NULL, 7).
    assert!(got.iter().any(|r| {
        cell(&r[0]).is_none() && cell(&r[1]).is_none() && as_i64(&r[2]) == 7
    }));
}

#[test]
fn grouping_sets_explicit_list() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE g2 (a TEXT, b TEXT, v INT)").unwrap();
    e.execute("INSERT INTO g2 VALUES ('x', 'p', 1), ('y', 'p', 2)")
        .unwrap();
    // Only per-a and per-b groupings — NO grand total (not listed).
    let got = rows(
        &mut e,
        "SELECT a, b, SUM(v) FROM g2 \
         GROUP BY GROUPING SETS ((a), (b)) \
         ORDER BY 1 NULLS LAST, 2 NULLS LAST",
    );
    assert_eq!(got.len(), 3); // x, y per-a + p per-b.
    assert!(got.iter().all(|r| !(cell(&r[0]).is_none() && cell(&r[1]).is_none())));
    // Per-b row: (NULL, p, 3).
    assert!(got.iter().any(|r| {
        cell(&r[0]).is_none()
            && cell(&r[1]).as_deref() == Some("p")
            && as_i64(&r[2]) == 3
    }));
}
