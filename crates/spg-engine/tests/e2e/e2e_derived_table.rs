//! v7.37.17 (17.6 siblings) — plain derived tables:
//! FROM ( SELECT … ) alias, riding the lateral_subquery channel.

use spg_engine::{Engine, QueryResult};

fn rows(e: &mut Engine, sql: &str) -> Vec<Vec<spg_storage::Value<'static>>> {
    let r = e
        .execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"));
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

#[test]
fn basic_derived_table() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (x INT)").unwrap();
    e.execute("INSERT INTO t VALUES (1), (2), (3)").unwrap();
    let got = rows(
        &mut e,
        "SELECT x FROM (SELECT x FROM t WHERE x > 1) sub ORDER BY x",
    );
    assert_eq!(got.len(), 2);
    assert_eq!(as_i64(&got[0][0]), 2);
    assert_eq!(as_i64(&got[1][0]), 3);
}

#[test]
fn union_inside_derived_table() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE u (x INT)").unwrap();
    e.execute("INSERT INTO u VALUES (1)").unwrap();
    // The #328 shape that exposed the gap.
    let got = rows(
        &mut e,
        "SELECT x FROM (SELECT x FROM u UNION ALL SELECT x + 10 FROM u) sub \
         ORDER BY x",
    );
    assert_eq!(got.len(), 2);
    assert_eq!(as_i64(&got[0][0]), 1);
    assert_eq!(as_i64(&got[1][0]), 11);
}

#[test]
fn column_alias_list_renames() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE ca (x INT, y INT)").unwrap();
    e.execute("INSERT INTO ca VALUES (1, 10), (2, 20)").unwrap();
    // AS t(a, b) renames positionally; the outer query addresses
    // the renamed columns.
    let got = rows(
        &mut e,
        "SELECT a, b FROM (SELECT x, y FROM ca) t(a, b) WHERE a = 2",
    );
    assert_eq!(got.len(), 1);
    assert_eq!(as_i64(&got[0][0]), 2);
    assert_eq!(as_i64(&got[0][1]), 20);
}

#[test]
fn derived_table_joins_a_real_table() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE facts (k INT, v TEXT)").unwrap();
    e.execute("INSERT INTO facts VALUES (1, 'one'), (2, 'two')")
        .unwrap();
    e.execute("CREATE TABLE keys (k INT)").unwrap();
    e.execute("INSERT INTO keys VALUES (2)").unwrap();
    let got = rows(
        &mut e,
        "SELECT f.v FROM (SELECT k FROM keys) sub \
         JOIN facts f ON f.k = sub.k",
    );
    assert_eq!(got.len(), 1);
    assert!(matches!(&got[0][0], spg_storage::Value::Text(s) if s == "two"));
}

#[test]
fn aggregate_over_derived_table() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE a (x INT)").unwrap();
    e.execute("INSERT INTO a VALUES (1), (2), (3), (4)")
        .unwrap();
    let got = rows(
        &mut e,
        "SELECT COUNT(*), SUM(x) FROM (SELECT x FROM a WHERE x > 1) big",
    );
    assert_eq!(as_i64(&got[0][0]), 3);
    assert_eq!(as_i64(&got[0][1]), 9);
}

#[test]
fn values_union_column_type_unification() {
    use spg_storage::Value;
    // PG resolves a VALUES / UNION column to one common type and casts
    // every branch to it (live PG18.4). SPG previously left each branch
    // its own type, so a date column seeded by only the first row's cast
    // came back DATE + TEXT. Verify the whole column is now uniform.
    let mut e = Engine::new();

    // DATE: first row cast, rest bare literals → all DATE.
    let got = rows(
        &mut e,
        "SELECT d FROM (VALUES('2020-01-01'::date),('2020-01-02'),('2020-01-05')) t(d) ORDER BY d",
    );
    assert!(
        got.iter().all(|r| matches!(&r[0], Value::Date(_))),
        "want all DATE, got {got:?}"
    );

    // Numeric widening: bigint ∪ int → all bigint.
    let got = rows(
        &mut e,
        "SELECT n FROM (VALUES(1::bigint),(2),(3)) t(n) ORDER BY n",
    );
    assert!(
        got.iter().all(|r| matches!(&r[0], Value::BigInt(_))),
        "want all BIGINT, got {got:?}"
    );

    // DATE ∪ TIMESTAMP → TIMESTAMP.
    let got = rows(
        &mut e,
        "SELECT x FROM (SELECT '2020-01-01'::date UNION ALL SELECT '2020-01-02 10:00'::timestamp) t(x)",
    );
    assert!(
        got.iter().all(|r| matches!(&r[0], Value::Timestamp(_))),
        "want all TIMESTAMP, got {got:?}"
    );

    // A value-based window frame over the unified DATE column now works
    // (this was the motivating regression).
    let got = rows(
        &mut e,
        "SELECT string_agg(c::text, ',') FROM (SELECT count(*) OVER \
         (ORDER BY d RANGE BETWEEN '1 day'::interval PRECEDING AND CURRENT ROW) c \
         FROM (VALUES('2020-01-01'::date),('2020-01-02'),('2020-01-02'),('2020-01-05')) t(d)) s",
    );
    assert!(
        matches!(&got[0][0], Value::Text(s) if s == "1,3,3,1"),
        "got {got:?}"
    );
}
