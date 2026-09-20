//! 9.0.0 — two of the partition catalog's own claims, measured against
//! PostgreSQL 18.6.
//!
//! **A unique constraint on a partitioned table must include every
//! partition-key column.** SPG accepted `UNIQUE (e)` on a table
//! partitioned by `id` and then enforced it PER PARTITION — which is
//! not what the declaration says: two partitions could each hold the
//! same `e`. PG refuses all three spellings (the inline constraint,
//! `ALTER TABLE … ADD CONSTRAINT`, `CREATE UNIQUE INDEX`) with one
//! sentence and a DETAIL naming the missing column.
//!
//! **A partition's index is named after the CHILD.** `CREATE INDEX
//! pt_v ON pt (v)` over a partition `pt1` gives `pt1_v_idx` on PG;
//! SPG suffixed the parent's name instead (`pt_v__pt1`), a name no
//! PostgreSQL client expects and the one a dump would restore under.

use spg_engine::{Engine, QueryResult};

fn col(e: &mut Engine, sql: &str) -> Vec<String> {
    let QueryResult::Rows { rows, .. } = e
        .execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"))
    else {
        panic!("expected rows for {sql}");
    };
    rows.into_iter()
        .map(|r| spg_engine::eval::value_to_text(r.values.first().expect("one column")))
        .collect()
}

fn err(e: &mut Engine, sql: &str) -> String {
    format!("{}", e.execute(sql).unwrap_err())
}

const HEAD: &str = "unique constraint on partitioned table must include all partitioning columns";

#[test]
fn a_unique_that_misses_a_partition_key_column_is_refused() {
    let mut e = Engine::new();
    for (sql, kind) in [
        (
            "CREATE TABLE q1 (id int, e int, UNIQUE (e)) PARTITION BY RANGE (id)",
            "UNIQUE",
        ),
        (
            "CREATE TABLE q2 (id int, e int, PRIMARY KEY (e)) PARTITION BY RANGE (id)",
            "PRIMARY KEY",
        ),
    ] {
        let got = err(&mut e, sql);
        assert!(got.contains(HEAD), "{sql}: {got}");
        assert!(
            got.contains(&format!(
                "{kind} constraint on table \"{}\" lacks column \"id\" \
                 which is part of the partition key.",
                if kind == "UNIQUE" { "q1" } else { "q2" }
            )),
            "{sql}: {got}"
        );
    }
    e.execute("CREATE TABLE q3 (id int, e int) PARTITION BY RANGE (id)")
        .unwrap();
    for sql in [
        "ALTER TABLE q3 ADD CONSTRAINT qu UNIQUE (e)",
        "CREATE UNIQUE INDEX qi ON q3 (e)",
    ] {
        let got = err(&mut e, sql);
        assert!(got.contains(HEAD), "{sql}: {got}");
        assert!(got.contains("lacks column \"id\""), "{sql}: {got}");
    }
}

#[test]
fn a_unique_that_covers_the_key_and_a_plain_index_are_accepted() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE q4 (id int, e int, UNIQUE (id, e)) PARTITION BY RANGE (id)")
        .unwrap();
    e.execute("CREATE TABLE q5 (id int, e int) PARTITION BY RANGE (id)")
        .unwrap();
    e.execute("CREATE UNIQUE INDEX q5u ON q5 (id, e)").unwrap();
    // A NON-unique index over a non-key column is fine on both engines.
    e.execute("CREATE INDEX q5p ON q5 (e)").unwrap();
}

#[test]
fn a_partitions_index_is_named_after_the_child() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE pt (id int, v int) PARTITION BY RANGE (id)")
        .unwrap();
    e.execute("CREATE TABLE pt1 PARTITION OF pt FOR VALUES FROM (0) TO (100)")
        .unwrap();
    e.execute("CREATE INDEX pt_v ON pt (v)").unwrap();
    assert_eq!(
        col(
            &mut e,
            "SELECT indexname FROM pg_indexes WHERE tablename='pt1' ORDER BY 1"
        ),
        vec!["pt1_v_idx".to_string()]
    );
    // And a child created AFTER the index inherits it under its own name.
    e.execute("CREATE TABLE pt2 PARTITION OF pt FOR VALUES FROM (100) TO (200)")
        .unwrap();
    assert_eq!(
        col(
            &mut e,
            "SELECT indexname FROM pg_indexes WHERE tablename='pt2' ORDER BY 1"
        ),
        vec!["pt2_v_idx".to_string()]
    );
}
