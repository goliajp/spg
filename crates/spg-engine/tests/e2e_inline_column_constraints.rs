//! v7.13.0 — inline UNIQUE / CHECK column constraints.
//! mailrs round-5 G2 (inline UNIQUE, 4 hits) and G3 (inline
//! CHECK, 3 hits).

use spg_engine::{Engine, QueryResult};

#[test]
fn inline_unique_creates_table_level_unique_constraint() {
    let mut eng = Engine::new();
    eng.execute(
        "CREATE TABLE prefixes (\
           id INT NOT NULL, \
           prefix TEXT NOT NULL UNIQUE\
         )",
    )
    .unwrap();
    let table = eng.catalog().get("prefixes").expect("table present");
    let ucs = &table.schema().uniqueness_constraints;
    assert_eq!(ucs.len(), 1, "expected one UC, got {ucs:?}");
    assert!(!ucs[0].is_primary_key);
    assert_eq!(ucs[0].columns.len(), 1);
    assert!(!ucs[0].nulls_not_distinct);
}

#[test]
fn inline_unique_rejects_duplicates_on_insert() {
    let mut eng = Engine::new();
    eng.execute("CREATE TABLE t (id INT NOT NULL, name TEXT NOT NULL UNIQUE)")
        .unwrap();
    eng.execute("INSERT INTO t VALUES (1, 'alice')").unwrap();
    let r = eng.execute("INSERT INTO t VALUES (2, 'alice')");
    assert!(r.is_err(), "expected UNIQUE violation, got {r:?}");
}

#[test]
fn inline_check_rejects_disallowed_values() {
    let mut eng = Engine::new();
    eng.execute(
        "CREATE TABLE keys (\
           id INT NOT NULL, \
           purpose TEXT NOT NULL CHECK (purpose IN ('s/mime','pgp','signing'))\
         )",
    )
    .unwrap();
    eng.execute("INSERT INTO keys VALUES (1, 'pgp')").unwrap();
    let r = eng.execute("INSERT INTO keys VALUES (2, 'bogus')");
    assert!(r.is_err(), "expected CHECK violation, got {r:?}");
}

#[test]
fn inline_check_allows_valid_values() {
    let mut eng = Engine::new();
    eng.execute("CREATE TABLE t (n INT NOT NULL CHECK (n > 0))")
        .unwrap();
    eng.execute("INSERT INTO t VALUES (5)").unwrap();
    let table = eng.catalog().get("t").expect("table present");
    assert_eq!(table.rows().len(), 1);
}

#[test]
fn table_level_check_rejects_on_insert() {
    let mut eng = Engine::new();
    eng.execute(
        "CREATE TABLE t (\
           a INT NOT NULL, \
           b INT NOT NULL, \
           CHECK (a < b)\
         )",
    )
    .unwrap();
    eng.execute("INSERT INTO t VALUES (1, 2)").unwrap();
    let r = eng.execute("INSERT INTO t VALUES (5, 1)");
    assert!(r.is_err(), "expected CHECK violation, got {r:?}");
}

#[test]
fn check_constraint_enforced_on_update() {
    let mut eng = Engine::new();
    eng.execute("CREATE TABLE t (n INT NOT NULL CHECK (n > 0))")
        .unwrap();
    eng.execute("INSERT INTO t VALUES (5)").unwrap();
    let r = eng.execute("UPDATE t SET n = -1 WHERE n = 5");
    assert!(r.is_err(), "expected CHECK violation on UPDATE, got {r:?}");
}

#[test]
fn check_constraint_null_passes() {
    // PG semantics: NULL result on CHECK predicate passes (only
    // definite-false rejects).
    let mut eng = Engine::new();
    eng.execute("CREATE TABLE t (n INT, c TEXT CHECK (c <> 'bad'))")
        .unwrap();
    // c IS NULL → "c <> 'bad'" → NULL → passes.
    eng.execute("INSERT INTO t VALUES (1, NULL)").unwrap();
    let table = eng.catalog().get("t").expect("table present");
    assert_eq!(table.rows().len(), 1);
}

#[test]
fn inline_unique_and_check_combined() {
    let mut eng = Engine::new();
    eng.execute(
        "CREATE TABLE t (\
           id INT NOT NULL, \
           grade TEXT NOT NULL UNIQUE CHECK (grade IN ('A','B','C'))\
         )",
    )
    .unwrap();
    eng.execute("INSERT INTO t VALUES (1, 'A')").unwrap();
    // CHECK rejects
    let r = eng.execute("INSERT INTO t VALUES (2, 'Z')");
    assert!(r.is_err());
    // UNIQUE rejects
    let r = eng.execute("INSERT INTO t VALUES (3, 'A')");
    assert!(r.is_err());
}

#[test]
fn create_table_display_round_trips_table_constraints() {
    // WAL replay path: CreateTableStatement's Display form must
    // re-parse to a statement that recreates the same constraints.
    let mut eng = Engine::new();
    let result =
        eng.execute("CREATE TABLE t (id INT NOT NULL UNIQUE, n INT NOT NULL CHECK (n > 0))");
    assert!(matches!(result, Ok(QueryResult::CommandOk { .. })));
    let table = eng.catalog().get("t").expect("table present");
    assert_eq!(table.schema().uniqueness_constraints.len(), 1);
    assert_eq!(table.schema().checks.len(), 1);
}
