//! 9.0.0 — a partitioned table's PRIMARY KEY is enforced, because the
//! CHILD holds it.
//!
//! The rows live in the children, so a key declared on the parent alone
//! was enforced nowhere. Measured against PostgreSQL 18.6:
//!
//! ```text
//!   CREATE TABLE pt(id int primary key, v text) PARTITION BY RANGE (id);
//!   CREATE TABLE pt1 PARTITION OF pt FOR VALUES FROM (0) TO (100);
//!   INSERT INTO pt1 VALUES (5,'a'); INSERT INTO pt1 VALUES (5,'b');
//!     PG 18.6    duplicate key value violates unique constraint "pt1_pkey"
//!     SPG 8.0.4  INSERT 0 1 -- and count(*) is 2
//!
//!   pg_indexes  PG 18.6   pt1_pkey on the child, pt_pkey on the parent
//!               SPG 8.0.4 pt_pkey alone
//! ```
//!
//! Both ways in are here: a child created with `PARTITION OF`, and one
//! created standalone and ATTACHed. Attaching a child that already holds
//! a duplicate is refused in PostgreSQL's own words.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn run(e: &mut Engine, sql: &str) {
    e.execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"));
}

fn refused(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql) {
        Err(err) => alloc_string(&err),
        Ok(_) => panic!("{sql}: accepted what PostgreSQL refuses"),
    }
}

fn alloc_string(err: &spg_engine::EngineError) -> String {
    format!("{err}")
}

fn count(e: &mut Engine, sql: &str) -> i64 {
    let QueryResult::Rows { rows, .. } = e
        .execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"))
    else {
        panic!("expected rows for {sql}");
    };
    match rows
        .into_iter()
        .next()
        .expect("one row")
        .values
        .into_iter()
        .next()
        .expect("one column")
    {
        Value::BigInt(n) => n,
        Value::Int(n) => i64::from(n),
        other => panic!("expected an integer, got {other:?}"),
    }
}

fn index_names(e: &mut Engine, table: &str) -> Vec<String> {
    let sql = format!("SELECT indexname FROM pg_indexes WHERE tablename = '{table}' ORDER BY 1");
    let QueryResult::Rows { rows, .. } = e
        .execute(&sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"))
    else {
        panic!("expected rows for {sql}");
    };
    rows.into_iter()
        .map(|r| match r.values.into_iter().next().expect("one column") {
            Value::Text(s) => s.to_string(),
            other => panic!("expected text, got {other:?}"),
        })
        .collect()
}

#[test]
fn a_partition_of_child_holds_the_parents_primary_key() {
    let mut e = Engine::new();
    run(
        &mut e,
        "CREATE TABLE pt(id int primary key, v text) PARTITION BY RANGE (id)",
    );
    run(
        &mut e,
        "CREATE TABLE pt1 PARTITION OF pt FOR VALUES FROM (0) TO (100)",
    );
    assert_eq!(index_names(&mut e, "pt1"), vec!["pt1_pkey".to_string()]);

    run(&mut e, "INSERT INTO pt1 VALUES (5,'a')");
    // Through the child …
    let m = refused(&mut e, "INSERT INTO pt1 VALUES (5,'b')");
    assert!(m.contains("pt1_pkey"), "{m}");
    // … and through the parent, which routes to the same child.
    let m = refused(&mut e, "INSERT INTO pt VALUES (5,'c')");
    assert!(m.contains("pt1_pkey"), "{m}");
    assert_eq!(count(&mut e, "SELECT count(*) FROM pt"), 1);

    // A different key in the same partition is still accepted, and one
    // in a sibling partition is unaffected.
    run(
        &mut e,
        "CREATE TABLE pt2 PARTITION OF pt FOR VALUES FROM (100) TO (200)",
    );
    run(&mut e, "INSERT INTO pt VALUES (6,'d'),(150,'e')");
    assert_eq!(count(&mut e, "SELECT count(*) FROM pt"), 3);
}

#[test]
fn an_attached_child_holds_the_parents_key_and_indexes() {
    let mut e = Engine::new();
    run(
        &mut e,
        "CREATE TABLE rt(id int not null, v text) PARTITION BY RANGE (id)",
    );
    run(&mut e, "ALTER TABLE rt ADD PRIMARY KEY (id)");
    run(&mut e, "CREATE INDEX rt_v ON rt(v)");

    run(&mut e, "CREATE TABLE rc1(id int not null, v text)");
    run(
        &mut e,
        "ALTER TABLE rt ATTACH PARTITION rc1 FOR VALUES FROM (0) TO (100)",
    );
    // The key AND the parent's plain index, both of which an attached
    // child got none of.
    let names = index_names(&mut e, "rc1");
    assert!(names.contains(&"rc1_pkey".to_string()), "{names:?}");
    assert_eq!(
        names.len(),
        2,
        "want the parent's plain index too: {names:?}"
    );

    run(&mut e, "INSERT INTO rc1 VALUES (5,'a')");
    let m = refused(&mut e, "INSERT INTO rc1 VALUES (5,'b')");
    assert!(m.contains("rc1_pkey"), "{m}");
}

#[test]
fn attaching_a_child_that_already_holds_a_duplicate_is_refused() {
    let mut e = Engine::new();
    run(
        &mut e,
        "CREATE TABLE rt(id int not null, v text) PARTITION BY RANGE (id)",
    );
    run(&mut e, "ALTER TABLE rt ADD PRIMARY KEY (id)");
    run(&mut e, "CREATE TABLE rc2(id int not null, v text)");
    run(&mut e, "INSERT INTO rc2 VALUES (105,'a'),(105,'b')");

    // PostgreSQL 18.6's own sentence, and it names the CHILD's index.
    let m = refused(
        &mut e,
        "ALTER TABLE rt ATTACH PARTITION rc2 FOR VALUES FROM (100) TO (200)",
    );
    assert!(
        m.contains("could not create unique index \"rc2_pkey\"")
            && m.contains("Key (id)=(105) is duplicated."),
        "{m}"
    );
    // And the refusal changed nothing: rc2 is still standalone.
    assert_eq!(count(&mut e, "SELECT count(*) FROM rc2"), 2);
    assert!(index_names(&mut e, "rc2").is_empty());
}
