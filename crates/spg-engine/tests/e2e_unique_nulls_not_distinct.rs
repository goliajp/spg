//! v7.13.0 — `UNIQUE NULLS NOT DISTINCT (cols)` table constraint.
//! mailrs round-5 G10 (PG 15+ surface).

use spg_engine::Engine;

#[test]
fn default_unique_treats_nulls_as_distinct() {
    let mut eng = Engine::new();
    eng.execute("CREATE TABLE t (a INT, b INT, UNIQUE (a, b))")
        .unwrap();
    eng.execute("INSERT INTO t VALUES (NULL, NULL)").unwrap();
    // SQL-standard NULLS DISTINCT — a second all-NULL row passes.
    eng.execute("INSERT INTO t VALUES (NULL, NULL)").unwrap();
    let table = eng.catalog().get("t").expect("table present");
    assert_eq!(table.rows().len(), 2);
}

#[test]
fn nulls_not_distinct_rejects_duplicate_null_rows() {
    let mut eng = Engine::new();
    eng.execute("CREATE TABLE t (a INT, b INT, UNIQUE NULLS NOT DISTINCT (a, b))")
        .unwrap();
    eng.execute("INSERT INTO t VALUES (NULL, NULL)").unwrap();
    let r = eng.execute("INSERT INTO t VALUES (NULL, NULL)");
    assert!(
        r.is_err(),
        "expected UNIQUE NULLS NOT DISTINCT collision, got {r:?}"
    );
}

#[test]
fn nulls_not_distinct_still_rejects_non_null_duplicates() {
    let mut eng = Engine::new();
    eng.execute(
        "CREATE TABLE accounts (\
           name TEXT, \
           domain TEXT, \
           UNIQUE NULLS NOT DISTINCT (name, domain)\
         )",
    )
    .unwrap();
    eng.execute("INSERT INTO accounts VALUES ('alice', 'example.com')")
        .unwrap();
    let r = eng.execute("INSERT INTO accounts VALUES ('alice', 'example.com')");
    assert!(r.is_err());
}

#[test]
fn nulls_not_distinct_persists_on_schema() {
    let mut eng = Engine::new();
    eng.execute("CREATE TABLE t (a INT, b INT, UNIQUE NULLS NOT DISTINCT (a, b))")
        .unwrap();
    let table = eng.catalog().get("t").expect("table present");
    let uc = &table.schema().uniqueness_constraints[0];
    assert!(uc.nulls_not_distinct);
}
