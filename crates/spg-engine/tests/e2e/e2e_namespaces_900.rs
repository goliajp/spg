//! 9.0.0 (C9) — `CREATE SCHEMA` made a NAME and nothing else.
//!
//! The qualifier on `sa.t` was dropped at parse time, so `sa.t` and
//! `sb.t` were ONE relation: the second `CREATE TABLE` answered
//! `relation "t" already exists`, and a two-schema application read the
//! other schema's rows. Measured 2026-09-21 against PostgreSQL 18.6,
//! which answers 1 row from each.
//!
//! A relation belongs to a schema now. The schema travels with the name
//! as one key, so every map keyed by a relation name carries it, and an
//! unqualified name is looked for along the session's `search_path` —
//! PostgreSQL's rule.

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

fn two_schemas() -> Engine {
    let mut e = Engine::new();
    run(&mut e, "CREATE SCHEMA c9a");
    run(&mut e, "CREATE SCHEMA c9b");
    run(&mut e, "CREATE TABLE c9a.t(id int)");
    run(&mut e, "CREATE TABLE c9b.t(id int)");
    run(&mut e, "INSERT INTO c9a.t VALUES (1)");
    run(&mut e, "INSERT INTO c9b.t VALUES (2)");
    e
}

/// The measurement at the top of this file.
#[test]
fn n_c9_two_schemas_are_two_relations() {
    let mut e = two_schemas();
    assert_eq!(
        vals(&mut e, "SELECT count(*), sum(id) FROM c9a.t"),
        vec!["1|1"]
    );
    assert_eq!(
        vals(&mut e, "SELECT count(*), sum(id) FROM c9b.t"),
        vec!["1|2"]
    );
}

/// An unqualified name is looked for along the search path.
#[test]
fn n_c9_search_path_decides_which_one_an_unqualified_name_reaches() {
    let mut e = two_schemas();
    run(&mut e, "CREATE TABLE t(id int)");
    run(&mut e, "INSERT INTO t VALUES (99)");
    // `public` is where an unqualified CREATE puts it, and where an
    // unqualified SELECT looks.
    assert_eq!(vals(&mut e, "SELECT sum(id) FROM t"), vec!["99"]);
    run(&mut e, "SET search_path TO c9b, public");
    assert_eq!(vals(&mut e, "SELECT sum(id) FROM t"), vec!["2"]);
    run(&mut e, "SET search_path TO c9a");
    assert_eq!(vals(&mut e, "SELECT sum(id) FROM t"), vec!["1"]);
    run(&mut e, "RESET search_path");
    assert_eq!(vals(&mut e, "SELECT sum(id) FROM t"), vec!["99"]);
}

/// `DROP SCHEMA … CASCADE` takes the schema's own relations and no others.
#[test]
fn n_c9_drop_schema_cascade_takes_only_its_own() {
    let mut e = two_schemas();
    run(&mut e, "DROP SCHEMA c9a CASCADE");
    assert_eq!(
        vals(&mut e, "SELECT count(*), sum(id) FROM c9b.t"),
        vec!["1|2"]
    );
    let err = e
        .execute("SELECT * FROM c9a.t")
        .expect_err("the schema and its table are gone");
    assert!(
        format!("{err}").contains("c9a.t"),
        "the sentence names the qualified relation: {err}"
    );
}

/// An unqualified CREATE puts the relation in the first schema on the
/// path — `current_schema()`, which is where PostgreSQL puts it.
#[test]
fn n_c9_create_follows_the_search_path() {
    let mut e = Engine::new();
    run(&mut e, "CREATE SCHEMA c9a");
    run(&mut e, "SET search_path TO c9a");
    run(&mut e, "CREATE TABLE t(id int)");
    run(&mut e, "INSERT INTO t VALUES (7)");
    assert_eq!(vals(&mut e, "SELECT current_schema()"), vec!["c9a"]);
    assert_eq!(vals(&mut e, "SELECT sum(id) FROM c9a.t"), vec!["7"]);
    assert_eq!(
        vals(
            &mut e,
            "SELECT table_schema FROM information_schema.tables WHERE table_name = 't'"
        ),
        vec!["c9a"]
    );
    run(&mut e, "SET search_path TO public");
    assert!(
        e.execute("SELECT sum(id) FROM t").is_err(),
        "it is not in `public`"
    );
    // …and `DROP TABLE t` under the path drops the one the path finds.
    run(&mut e, "SET search_path TO c9a");
    run(&mut e, "DROP TABLE t");
    assert!(e.execute("SELECT sum(id) FROM c9a.t").is_err());
}

/// `ALTER TABLE … SET SCHEMA` moves the relation.
#[test]
fn n_c9_set_schema_moves_it() {
    let mut e = two_schemas();
    run(&mut e, "CREATE TABLE moving(id int)");
    run(&mut e, "INSERT INTO moving VALUES (5)");
    run(&mut e, "ALTER TABLE moving SET SCHEMA c9a");
    assert_eq!(vals(&mut e, "SELECT sum(id) FROM c9a.moving"), vec!["5"]);
    assert!(e.execute("SELECT sum(id) FROM moving").is_err());
}

/// The catalogs say which schema each relation is in.
#[test]
fn n_c9_the_catalogs_name_the_schema() {
    let mut e = two_schemas();
    assert_eq!(
        vals(
            &mut e,
            "SELECT nspname FROM pg_namespace WHERE nspname LIKE 'c9%' ORDER BY 1"
        ),
        vec!["c9a", "c9b"]
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT schema_name FROM information_schema.schemata \
             WHERE schema_name LIKE 'c9%' ORDER BY 1"
        ),
        vec!["c9a", "c9b"]
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT table_schema, table_name FROM information_schema.tables \
             WHERE table_name = 't' ORDER BY 1"
        ),
        vec!["c9a|t", "c9b|t"]
    );
    // `pg_class.relname` is the BARE name and `relnamespace` joins.
    assert_eq!(
        vals(
            &mut e,
            "SELECT n.nspname, c.relname FROM pg_class c \
             JOIN pg_namespace n ON n.oid = c.relnamespace \
             WHERE c.relname = 't' ORDER BY 1"
        ),
        vec!["c9a|t", "c9b|t"]
    );
}

/// A schema that does not exist is PostgreSQL's refusal, not a relation
/// created out of thin air.
#[test]
fn n_c9_an_unknown_schema_is_refused() {
    let mut e = Engine::new();
    let err = e
        .execute("CREATE TABLE nosuch.t(id int)")
        .expect_err("no such schema");
    assert!(
        format!("{err}").contains("schema \"nosuch\" does not exist"),
        "PG 18.6's sentence: {err}"
    );
}
