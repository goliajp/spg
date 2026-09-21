//! 9.0.0 (C8) — `CREATE DATABASE` made a NAME, and nothing else.
//!
//! A table made in one database was visible from another: measured
//! 2026-09-21, `CREATE DATABASE c8a; CREATE DATABASE c8b;` then a table
//! in `c8a` answered `1` from `c8b`, where PostgreSQL 18.6 answers `0`.
//! Same family as C9 and the same shape one level up — a database made
//! here owns its relations.
//!
//! The datadir's own database keeps the shorter key, so a deployment
//! that never ran `CREATE DATABASE` — which is every deployment today —
//! reaches its tables exactly as before. That is also why a connection
//! to a name nobody created is the datadir's own database rather than
//! PostgreSQL's `FATAL: database "x" does not exist`: recorded as a
//! known difference, not closed here.

use spg_engine::{Engine, QueryResult};

fn vals(e: &mut Engine, sql: &str) -> Vec<String> {
    match e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err}")) {
        QueryResult::Rows { rows, .. } => rows
            .iter()
            .map(|r| {
                r.values
                    .iter()
                    .map(spg_engine::eval::value_to_text)
                    .collect::<Vec<_>>()
                    .join("|")
            })
            .collect(),
        other => panic!("{sql}: {other:?}"),
    }
}

fn run(e: &mut Engine, sql: &str) {
    e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err}"));
}

fn two_databases() -> Engine {
    let mut e = Engine::new();
    run(&mut e, "CREATE DATABASE c8a");
    run(&mut e, "CREATE DATABASE c8b");
    run(&mut e, "SET spg.database = 'c8a'");
    run(&mut e, "CREATE TABLE c8t(id int)");
    run(&mut e, "INSERT INTO c8t VALUES (1)");
    e
}

/// The measurement at the top of this file.
#[test]
fn n_c8_a_table_is_not_visible_from_another_database() {
    let mut e = two_databases();
    assert_eq!(vals(&mut e, "SELECT sum(id) FROM c8t"), vec!["1"]);
    run(&mut e, "SET spg.database = 'c8b'");
    assert!(
        e.execute("SELECT sum(id) FROM c8t").is_err(),
        "`c8b` does not hold `c8a`'s table"
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT count(*) FROM information_schema.tables WHERE table_name = 'c8t'"
        ),
        vec!["0"]
    );
    // Each database may hold a relation of the same name, with its own
    // rows.
    run(&mut e, "CREATE TABLE c8t(id int)");
    run(&mut e, "INSERT INTO c8t VALUES (2)");
    assert_eq!(vals(&mut e, "SELECT sum(id) FROM c8t"), vec!["2"]);
    run(&mut e, "SET spg.database = 'c8a'");
    assert_eq!(vals(&mut e, "SELECT sum(id) FROM c8t"), vec!["1"]);
}

/// The datadir's own database is what an uncreated name reaches, so a
/// deployment that never ran `CREATE DATABASE` is unchanged.
#[test]
fn n_c8_an_uncreated_name_is_the_datadirs_own_database() {
    let mut e = Engine::new();
    run(&mut e, "CREATE TABLE plain(id int)");
    run(&mut e, "INSERT INTO plain VALUES (3)");
    run(&mut e, "SET spg.database = 'whatever'");
    assert_eq!(vals(&mut e, "SELECT sum(id) FROM plain"), vec!["3"]);
    run(&mut e, "SET spg.database = 'anything_else'");
    assert_eq!(vals(&mut e, "SELECT sum(id) FROM plain"), vec!["3"]);
}

/// Schemas work inside a database of their own.
#[test]
fn n_c8_a_database_has_its_own_schemas() {
    let mut e = two_databases();
    run(&mut e, "CREATE SCHEMA s8");
    run(&mut e, "CREATE TABLE s8.inner_t(id int)");
    run(&mut e, "INSERT INTO s8.inner_t VALUES (4)");
    assert_eq!(vals(&mut e, "SELECT sum(id) FROM s8.inner_t"), vec!["4"]);
    run(&mut e, "SET spg.database = 'c8b'");
    assert!(
        e.execute("SELECT sum(id) FROM s8.inner_t").is_err(),
        "`c8b` does not hold `c8a`'s schema-qualified table either"
    );
}

/// `DROP DATABASE` takes the database's relations with it.
#[test]
fn n_c8_drop_database_takes_its_relations() {
    let mut e = two_databases();
    run(&mut e, "SET spg.database = 'c8b'");
    run(&mut e, "DROP DATABASE c8a");
    let names = vals(&mut e, "SELECT datname FROM pg_database ORDER BY 1");
    assert!(!names.iter().any(|n| n == "c8a"), "{names:?}");
    // …and the relation it held is gone, not left behind for a database
    // of the same name to inherit.
    run(&mut e, "CREATE DATABASE c8a");
    run(&mut e, "SET spg.database = 'c8a'");
    assert!(
        e.execute("SELECT sum(id) FROM c8t").is_err(),
        "a new database of the same name starts empty"
    );
}

/// `pg_database` lists the databases, and `current_database()` answers
/// the one this session is on.
#[test]
fn n_c8_the_catalog_lists_the_databases() {
    let mut e = two_databases();
    assert_eq!(vals(&mut e, "SELECT current_database()"), vec!["c8a"]);
    let names = vals(&mut e, "SELECT datname FROM pg_database ORDER BY 1");
    assert!(names.iter().any(|n| n == "c8a"), "{names:?}");
    assert!(names.iter().any(|n| n == "c8b"), "{names:?}");
}
