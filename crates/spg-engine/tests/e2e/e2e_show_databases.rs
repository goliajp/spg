//! v7.17.0 Phase 3.P0-58 — `SHOW DATABASES` / `SHOW SCHEMAS`.
//!
//! v7.39.2 — the list is no longer a fixed one.
//!
//! It was five hard-coded names, so a database this server had just been
//! asked to create was absent from it while `pg_database` — which the
//! PostgreSQL wire answers the same question with — listed it. That is
//! the defect sentori reported against 7.38.18 for `pg_database` itself;
//! the MySQL spelling of the question kept its canned answer.
//!
//! Two assertions changed with it, and both were about the canned list
//! rather than about a rule: that `postgres` is always present (it is
//! present when a connection NAMES it, which is what `pg_database` does
//! too) and that there are exactly five rows.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn rows(r: QueryResult) -> Vec<Vec<Value<'static>>> {
    match r {
        QueryResult::Rows { rows, .. } => rows.into_iter().map(|r| r.values).collect(),
        _ => panic!("expected rows"),
    }
}

fn names(e: &mut Engine, sql: &str) -> Vec<String> {
    rows(e.execute(sql).unwrap())
        .into_iter()
        .map(|row| match row[0].clone() {
            Value::Text(s) => s.into_owned(),
            other => panic!("expected Text, got {other:?}"),
        })
        .collect()
}

#[test]
fn show_databases_carries_mysqls_system_schemas() {
    let mut e = Engine::new();
    let got = names(&mut e, "SHOW DATABASES");
    for want in ["information_schema", "mysql", "performance_schema", "sys"] {
        assert!(
            got.contains(&want.to_string()),
            "{want} missing from {got:?}"
        );
    }
    // And the database this session is on, which is what
    // `current_database()` answers.
    assert!(got.contains(&"spg".to_string()), "{got:?}");
}

#[test]
fn a_created_database_appears_in_it() {
    // The defect. `pg_database` listed it and this did not.
    let mut e = Engine::new();
    assert!(!names(&mut e, "SHOW DATABASES").contains(&"made_here".to_string()));
    e.execute("CREATE DATABASE made_here").expect("create");
    assert!(
        names(&mut e, "SHOW DATABASES").contains(&"made_here".to_string()),
        "a database this server was asked to create must be listed"
    );
}

#[test]
fn the_two_wires_list_the_same_databases() {
    // The point of reading from one place: `pg_database` and `SHOW
    // DATABASES` answer the same question, and they disagreed.
    let mut e = Engine::new();
    e.execute("CREATE DATABASE both_see_me").expect("create");
    let shown = names(&mut e, "SHOW DATABASES");
    let listed = names(&mut e, "SELECT datname FROM pg_database ORDER BY 1");
    for db in &listed {
        assert!(
            shown.contains(db),
            "{db} in pg_database but not in SHOW DATABASES"
        );
    }
}

#[test]
fn show_schemas_is_alias() {
    let mut e = Engine::new();
    assert_eq!(
        names(&mut e, "SHOW SCHEMAS"),
        names(&mut e, "SHOW DATABASES")
    );
}

#[test]
fn schemata_lists_databases_for_mysql_and_namespaces_for_postgres() {
    // v7.39.2 — in MySQL a schema IS a database and this view lists
    // databases; SPG answered PostgreSQL's three namespaces to both
    // wires. A client asking "does database X exist" here got `public`,
    // `pg_catalog` and `information_schema` — and a different answer
    // from `SHOW DATABASES`, which is the same question spelled the
    // other way.
    let mut m = Engine::new();
    m.set_mysql_dialect(true);
    m.execute("CREATE DATABASE seen_by_both").expect("create");
    let schemata = names(
        &mut m,
        "SELECT schema_name FROM information_schema.schemata",
    );
    assert_eq!(
        schemata,
        names(&mut m, "SHOW DATABASES"),
        "one question, two spellings, one answer"
    );
    assert!(
        schemata.contains(&"seen_by_both".to_string()),
        "{schemata:?}"
    );
    assert!(
        !schemata.contains(&"public".to_string()),
        "MySQL has no `public`"
    );
    // MySQL's catalog column is `def`; PostgreSQL's is the database.
    assert_eq!(
        names(
            &mut m,
            "SELECT catalog_name FROM information_schema.schemata LIMIT 1"
        ),
        vec!["def".to_string()]
    );

    // The negative control: a PostgreSQL session keeps its namespaces.
    let mut p = Engine::new();
    let pg = names(
        &mut p,
        "SELECT schema_name FROM information_schema.schemata",
    );
    assert!(pg.contains(&"public".to_string()), "{pg:?}");
    assert!(pg.contains(&"pg_catalog".to_string()), "{pg:?}");
    assert!(!pg.contains(&"mysql".to_string()), "{pg:?}");
    assert_eq!(
        names(
            &mut p,
            "SELECT catalog_name FROM information_schema.schemata LIMIT 1"
        ),
        vec!["spg".to_string()]
    );
}
