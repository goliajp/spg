//! 9.0.0 — `ALTER TABLE … ADD CONSTRAINT … { PRIMARY KEY | UNIQUE }
//! USING INDEX <name>`.
//!
//! Reported by sentori (§5.2): the whole clause was a syntax error. It
//! is how a table gets a primary key without locking out writers — the
//! index is built `CONCURRENTLY` first and adopted afterwards — and how
//! `pg_dump` from some tools writes one back.
//!
//! PostgreSQL 18.6 renames the index to the constraint's name and says
//! so, and refuses four kinds of index. Measured, message for message.

use spg_engine::{Engine, QueryResult};

fn run(e: &mut Engine, sql: &str) {
    e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err}"));
}

fn rows(e: &mut Engine, sql: &str) -> Vec<String> {
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

fn err(e: &mut Engine, sql: &str) -> String {
    format!("{}", e.execute(sql).expect_err(sql))
}

#[test]
fn a_primary_key_adopts_an_existing_unique_index() {
    let mut e = Engine::new();
    run(&mut e, "CREATE TABLE a3(id int not null, e text)");
    run(&mut e, "CREATE UNIQUE INDEX a3_uq ON a3(id)");
    run(
        &mut e,
        "ALTER TABLE a3 ADD CONSTRAINT a3_pk PRIMARY KEY USING INDEX a3_uq",
    );
    // PG 18.6: the constraint is there and the index carries its name.
    assert_eq!(
        rows(
            &mut e,
            "SELECT conname, contype FROM pg_constraint \
             WHERE conrelid = 'a3'::regclass AND contype = 'p'"
        ),
        vec!["a3_pk|p"]
    );
    assert_eq!(
        rows(
            &mut e,
            "SELECT indexname FROM pg_indexes WHERE tablename = 'a3' ORDER BY 1"
        ),
        vec!["a3_pk"]
    );
    // And it enforces.
    run(&mut e, "INSERT INTO a3 VALUES (1, 'a')");
    let dup = err(&mut e, "INSERT INTO a3 VALUES (1, 'b')");
    assert!(
        dup.contains("duplicate key") || dup.contains("unique"),
        "{dup}"
    );
}

#[test]
fn a_unique_constraint_adopts_one_too() {
    let mut e = Engine::new();
    run(&mut e, "CREATE TABLE a3(id int not null, e text)");
    run(&mut e, "CREATE UNIQUE INDEX a3_uq2 ON a3(e)");
    run(
        &mut e,
        "ALTER TABLE a3 ADD CONSTRAINT a3_u2 UNIQUE USING INDEX a3_uq2",
    );
    assert_eq!(
        rows(
            &mut e,
            "SELECT conname, contype FROM pg_constraint \
             WHERE conrelid = 'a3'::regclass AND contype = 'u'"
        ),
        vec!["a3_u2|u"]
    );
}

/// The four indexes PostgreSQL refuses, each with its own sentence.
#[test]
fn the_index_must_be_unique_whole_and_over_columns() {
    let mut e = Engine::new();
    run(&mut e, "CREATE TABLE a3(id int not null, e text, f int)");

    run(&mut e, "CREATE INDEX a3_nonuq ON a3(f)");
    let m = err(
        &mut e,
        "ALTER TABLE a3 ADD CONSTRAINT a3_bad UNIQUE USING INDEX a3_nonuq",
    );
    assert!(m.contains("\"a3_nonuq\" is not a unique index"), "{m}");
    assert!(
        m.contains("Cannot create a primary key or unique constraint using such an index."),
        "{m}"
    );

    let m = err(
        &mut e,
        "ALTER TABLE a3 ADD CONSTRAINT a3_bad2 UNIQUE USING INDEX nosuchindex",
    );
    assert!(m.contains("index \"nosuchindex\" does not exist"), "{m}");

    run(&mut e, "CREATE UNIQUE INDEX a3_part ON a3(f) WHERE f > 0");
    let m = err(
        &mut e,
        "ALTER TABLE a3 ADD CONSTRAINT a3_bad3 UNIQUE USING INDEX a3_part",
    );
    assert!(m.contains("\"a3_part\" is a partial index"), "{m}");

    run(&mut e, "CREATE UNIQUE INDEX a3_expr ON a3(lower(e))");
    let m = err(
        &mut e,
        "ALTER TABLE a3 ADD CONSTRAINT a3_bad4 UNIQUE USING INDEX a3_expr",
    );
    assert!(m.contains("index \"a3_expr\" contains expressions"), "{m}");
}

/// The ordinary spelling still builds its own index.
#[test]
fn a_constraint_without_the_clause_is_unchanged() {
    let mut e = Engine::new();
    run(&mut e, "CREATE TABLE a4(id int not null, e text)");
    run(
        &mut e,
        "ALTER TABLE a4 ADD CONSTRAINT a4_pk PRIMARY KEY (id)",
    );
    assert_eq!(
        rows(
            &mut e,
            "SELECT conname FROM pg_constraint \
             WHERE conrelid = 'a4'::regclass AND contype = 'p'"
        ),
        vec!["a4_pk"]
    );
}
