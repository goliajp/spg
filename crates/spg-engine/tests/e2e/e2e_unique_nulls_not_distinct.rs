//! v7.38 (read01 P4.19) — UNIQUE ... NULLS NOT DISTINCT (PG 15+) treats NULL
//! keys as equal, so only one NULL is allowed; the default (NULLS DISTINCT)
//! keeps allowing many NULLs. Verified vs live PG 18.4.

use spg_engine::Engine;

#[test]
fn unique_nulls_not_distinct_rejects_second_null() {
    let mut e = Engine::new();

    // Column-inline NULLS NOT DISTINCT — a second NULL collides.
    e.execute("CREATE TABLE c(x int UNIQUE NULLS NOT DISTINCT)")
        .unwrap();
    e.execute("INSERT INTO c VALUES (NULL)").unwrap();
    assert!(e.execute("INSERT INTO c VALUES (NULL)").is_err());
    // Non-null uniqueness still holds.
    e.execute("INSERT INTO c VALUES (5)").unwrap();
    assert!(e.execute("INSERT INTO c VALUES (5)").is_err());

    // Table-level NULLS NOT DISTINCT behaves the same.
    e.execute("CREATE TABLE t(x int, UNIQUE NULLS NOT DISTINCT (x))")
        .unwrap();
    e.execute("INSERT INTO t VALUES (NULL)").unwrap();
    assert!(e.execute("INSERT INTO t VALUES (NULL)").is_err());
}

#[test]
fn unique_default_nulls_distinct_allows_many_nulls() {
    let mut e = Engine::new();
    // Default (and explicit NULLS DISTINCT) allow multiple NULLs.
    e.execute("CREATE TABLE d(x int UNIQUE)").unwrap();
    e.execute("INSERT INTO d VALUES (NULL)").unwrap();
    e.execute("INSERT INTO d VALUES (NULL)").unwrap();

    e.execute("CREATE TABLE d2(x int UNIQUE NULLS DISTINCT)")
        .unwrap();
    e.execute("INSERT INTO d2 VALUES (NULL)").unwrap();
    e.execute("INSERT INTO d2 VALUES (NULL)").unwrap();
}
