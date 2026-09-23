//! 9.0.4 — a UNIQUE or PRIMARY KEY added to a populated table is
//! checked against the rows that are already there.
//!
//! Measured on PostgreSQL 18.6:
//!
//! ```text
//!   ALTER TABLE a ADD CONSTRAINT au UNIQUE (v)     -- v repeats
//!   PG 18.6    ERROR: could not create unique index "au"
//!              DETAIL: Key (v)=(2) is duplicated.
//!   SPG 9.0.3  ALTER TABLE  -- and both rows stay
//!
//!   ALTER TABLE b ADD CONSTRAINT bp PRIMARY KEY (v)  -- v holds a NULL
//!   PG 18.6    ERROR: column "v" of relation "b" contains null values
//!   SPG 9.0.3  ALTER TABLE
//! ```
//!
//! What it costs an application: the constraint is what it was told it
//! could rely on. An upsert written as "this can only match one row", a
//! join written as "this side is unique", a dedup that stopped running
//! because the database enforces it — each of those is wrong for as
//! long as the duplicates stay, and nothing ever says so.
//!
//! `CREATE UNIQUE INDEX` over the same rows was already refused, which
//! is the tell: the check existed and one of the two routes to it did
//! not call it.

use spg_engine::Engine;

fn err_of(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql) {
        Ok(_) => String::new(),
        Err(x) => format!("{x:?}"),
    }
}

#[test]
fn unique_is_refused_when_the_rows_already_repeat() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE a (id int, v int)").unwrap();
    e.execute("INSERT INTO a VALUES (1, 2), (2, 2), (3, 3)")
        .unwrap();

    let said = err_of(&mut e, "ALTER TABLE a ADD CONSTRAINT au UNIQUE (v)");
    assert!(
        said.contains("could not create unique index"),
        "PostgreSQL refuses this; SPG said {said:?}"
    );
    assert!(
        said.contains("au") && said.contains("is duplicated"),
        "the refusal has to name the constraint and the key: {said:?}"
    );

    // And it did not install anything on the way out: a row that does
    // NOT collide still goes in.
    e.execute("INSERT INTO a VALUES (4, 2)")
        .expect("no constraint should have been installed");
}

#[test]
fn a_primary_key_is_refused_over_a_column_holding_a_null() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE b (id int, v int)").unwrap();
    e.execute("INSERT INTO b VALUES (1, NULL), (2, 2)").unwrap();

    let said = err_of(&mut e, "ALTER TABLE b ADD CONSTRAINT bp PRIMARY KEY (v)");
    assert!(
        said.contains("contains null values") && said.contains('v'),
        "PostgreSQL names the column; SPG said {said:?}"
    );

    // A UNIQUE over the same column is fine — NULLs are distinct — and
    // that is the line the fix must not cross.
    e.execute("ALTER TABLE b ADD CONSTRAINT bu UNIQUE (v)")
        .expect("NULLs do not collide under UNIQUE");
}

#[test]
fn a_constraint_the_rows_satisfy_still_goes_in() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE c (id int, v int NOT NULL)")
        .unwrap();
    e.execute("INSERT INTO c VALUES (1, 10), (2, 20)").unwrap();
    e.execute("ALTER TABLE c ADD CONSTRAINT cu UNIQUE (v)")
        .expect("the rows all differ");
    e.execute("ALTER TABLE c ADD CONSTRAINT cp PRIMARY KEY (id)")
        .expect("the ids all differ and none is null");
    // And it is enforced from here on.
    let said = err_of(&mut e, "INSERT INTO c VALUES (3, 10)");
    assert!(
        !said.is_empty(),
        "the installed UNIQUE has to reject a repeat"
    );
}
