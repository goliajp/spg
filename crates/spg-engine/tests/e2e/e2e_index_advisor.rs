//! v6.8.3 — `EXPLAIN (SUGGEST) <SELECT>` index advisor.
//!
//! Emits a `SUGGEST: CREATE INDEX … ON … (col)` line per
//! WHERE / JOIN column that lacks an index on its owning table.
//! Pure-syntax heuristic: no cardinality estimation, no
//! cost-based ranking. The intent is "tell operators where
//! indexes are missing"; pickier prioritisation lands in a
//! future v6.x.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn rows_of(r: QueryResult) -> Vec<Vec<Value<'static>>> {
    match r {
        QueryResult::Rows { rows, .. } => rows.into_iter().map(|r| r.values).collect(),
        other => panic!("expected Rows, got {other:?}"),
    }
}

fn join_lines(rows: Vec<Vec<Value<'static>>>) -> String {
    let mut s = String::new();
    for r in rows {
        if let Some(Value::Text(t)) = r.into_iter().next() {
            s.push_str(&t);
            s.push('\n');
        }
    }
    s
}

#[test]
fn suggest_emits_for_unindexed_where_column() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT NOT NULL, name TEXT NOT NULL)")
        .unwrap();
    // No index on `name`.
    let body = join_lines(rows_of(
        e.execute("EXPLAIN (SUGGEST) SELECT * FROM t WHERE name = 'alice'")
            .unwrap(),
    ));
    assert!(
        body.contains("SUGGEST:") && body.contains("ON t (name)"),
        "expected SUGGEST line for `name`; got:\n{body}"
    );
}

#[test]
fn suggest_skips_indexed_columns() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT NOT NULL, name TEXT NOT NULL)")
        .unwrap();
    e.execute("CREATE INDEX by_name ON t (name)").unwrap();
    let body = join_lines(rows_of(
        e.execute("EXPLAIN (SUGGEST) SELECT * FROM t WHERE name = 'alice'")
            .unwrap(),
    ));
    assert!(
        !body.contains("SUGGEST:"),
        "no SUGGEST line should fire when `name` is already indexed; got:\n{body}"
    );
}

#[test]
fn suggest_dedupes_repeated_columns() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT NOT NULL, name TEXT NOT NULL)")
        .unwrap();
    let body = join_lines(rows_of(
        e.execute("EXPLAIN (SUGGEST) SELECT * FROM t WHERE name = 'a' OR name = 'b' OR name = 'c'")
            .unwrap(),
    ));
    let suggest_count = body.lines().filter(|l| l.contains("SUGGEST:")).count();
    assert_eq!(
        suggest_count, 1,
        "should emit exactly one suggestion per (table, column); got:\n{body}"
    );
}

#[test]
fn suggest_covers_join_on_columns() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE orders (id INT NOT NULL, customer_id INT NOT NULL)")
        .unwrap();
    e.execute("CREATE TABLE customers (id INT NOT NULL, name TEXT NOT NULL)")
        .unwrap();
    // Neither customer_id (on orders) nor id (on customers, for
    // the join) has an index.
    let body = join_lines(rows_of(
        e.execute(
            "EXPLAIN (SUGGEST) SELECT * FROM orders JOIN customers \
             ON orders.customer_id = customers.id",
        )
        .unwrap(),
    ));
    assert!(
        body.contains("ON orders (customer_id)"),
        "expected SUGGEST for orders.customer_id; got:\n{body}"
    );
    assert!(
        body.contains("ON customers (id)"),
        "expected SUGGEST for customers.id; got:\n{body}"
    );
}

#[test]
fn suggest_emits_nothing_when_no_where() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT NOT NULL)").unwrap();
    let body = join_lines(rows_of(
        e.execute("EXPLAIN (SUGGEST) SELECT * FROM t").unwrap(),
    ));
    assert!(
        !body.contains("SUGGEST:"),
        "no WHERE / JOIN → no suggestion; got:\n{body}"
    );
}

#[test]
fn explain_without_suggest_keeps_legacy_behaviour() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT NOT NULL, name TEXT NOT NULL)")
        .unwrap();
    let body = join_lines(rows_of(
        e.execute("EXPLAIN SELECT * FROM t WHERE name = 'alice'")
            .unwrap(),
    ));
    assert!(
        !body.contains("SUGGEST:"),
        "EXPLAIN without (SUGGEST) must not emit advisor lines"
    );
}

#[test]
fn explain_suggest_round_trips_display() {
    use spg_sql::ast::Statement;
    use spg_sql::parser::parse_statement;
    let sql = "EXPLAIN (SUGGEST) SELECT * FROM t WHERE name = 'alice'";
    let stmt = parse_statement(sql).unwrap();
    let Statement::Explain(ref e) = stmt else {
        panic!("expected Explain");
    };
    assert!(e.suggest);
    assert!(!e.analyze);
    let stmt2 = parse_statement(&stmt.to_string()).unwrap();
    assert_eq!(stmt2, stmt);
}
