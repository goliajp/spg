//! 9.0.4 — you cannot remove or reshape something another object is
//! still using.
//!
//! Four steps of the acceptance panel, all the same family. Measured on
//! PostgreSQL 18.6, with what SPG 9.0.3 did beside each:
//!
//! ```text
//!   DROP TABLE mp           (m has a FOREIGN KEY to mp)
//!     PG   2BP01 cannot drop table mp because other objects depend on it
//!     SPG  DROP TABLE  — and m keeps a key pointing at nothing
//!   TRUNCATE mp             (same)
//!     PG   0A000 cannot truncate a table referenced in a foreign key constraint
//!     SPG  TRUNCATE  — and every row of m stops satisfying its key
//!   ALTER TABLE w DROP COLUMN v      (a view selects v)
//!     PG   2BP01 ...     SPG  the same sentence under 42000
//!   ALTER TABLE w ALTER COLUMN v TYPE bigint   (a view selects v)
//!     PG   0A000 cannot alter type of a column used by a view or rule
//!     SPG  ALTER TABLE  — the view now answers a type it was not made with
//! ```
//!
//! The first two lose referential integrity with nothing said, which is
//! the reason they are pinned separately from their SQLSTATEs.

use spg_engine::Engine;

fn err_of(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql) {
        Ok(_) => String::new(),
        Err(x) => format!("{x:?}"),
    }
}

fn with_fk() -> Engine {
    let mut e = Engine::new();
    e.execute("CREATE TABLE mp (id int PRIMARY KEY)").unwrap();
    e.execute("INSERT INTO mp VALUES (1), (2)").unwrap();
    e.execute("CREATE TABLE m (id int PRIMARY KEY, v int REFERENCES mp (id))")
        .unwrap();
    e.execute("INSERT INTO m VALUES (1, 1)").unwrap();
    e
}

#[test]
fn a_table_a_foreign_key_points_at_cannot_be_dropped() {
    let mut e = with_fk();
    let said = err_of(&mut e, "DROP TABLE mp");
    assert!(
        said.contains("because other objects depend on it"),
        "PostgreSQL refuses this; SPG said {said:?}"
    );
    assert!(
        said.contains("m_v_fkey") && said.contains("depends on table mp"),
        "the DETAIL names the constraint and the table: {said:?}"
    );
    // It is still there, and still enforcing.
    assert!(!err_of(&mut e, "INSERT INTO m VALUES (2, 9)").is_empty());
}

#[test]
fn cascade_drops_the_table_and_the_key_that_pointed_at_it() {
    let mut e = with_fk();
    e.execute("DROP TABLE mp CASCADE")
        .expect("CASCADE takes the dependent constraint with it");
    // The child survives, and the key that referenced the gone table is
    // gone too — otherwise the next insert would look up a table that
    // no longer exists.
    e.execute("INSERT INTO m VALUES (2, 9)")
        .expect("the foreign key went with the table");
}

#[test]
fn both_sides_named_in_one_statement_go_together() {
    // `DROP TABLE parent, child` is legal in PostgreSQL: the dependency
    // is satisfied because the dependent is going too. The first
    // version of the refusal did not look at the rest of the statement
    // and would have broken every teardown script that names both.
    let mut e = with_fk();
    e.execute("DROP TABLE mp, m")
        .expect("both sides in one statement");
    assert!(!err_of(&mut e, "SELECT 1 FROM m").is_empty());
}

#[test]
fn a_table_a_foreign_key_points_at_cannot_be_truncated() {
    let mut e = with_fk();
    let said = err_of(&mut e, "TRUNCATE mp");
    assert!(
        said.contains("cannot truncate a table referenced in a foreign key constraint"),
        "PostgreSQL refuses this; SPG said {said:?}"
    );
    assert_eq!(
        e.execute("SELECT count(*) FROM mp")
            .map(|_| ())
            .map_err(|x| format!("{x:?}")),
        Ok(())
    );

    // Naming both sides in one statement is fine — they empty together.
    e.execute("TRUNCATE m, mp")
        .expect("both sides in one statement");

    // And CASCADE pulls the referencing table in.
    let mut e2 = with_fk();
    e2.execute("TRUNCATE mp CASCADE")
        .expect("CASCADE empties the referencing table too");
}

#[test]
fn a_column_a_view_reads_cannot_change_type_or_be_dropped() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE w (id int, v int)").unwrap();
    e.execute("INSERT INTO w VALUES (1, 1)").unwrap();
    e.execute("CREATE VIEW wv AS SELECT id, v FROM w").unwrap();

    let retype = err_of(&mut e, "ALTER TABLE w ALTER COLUMN v TYPE bigint");
    assert!(
        retype.contains("cannot alter type of a column used by a view or rule"),
        "PostgreSQL refuses this; SPG said {retype:?}"
    );
    assert!(
        retype.contains("wv"),
        "the DETAIL names the view: {retype:?}"
    );

    let drop = err_of(&mut e, "ALTER TABLE w DROP COLUMN v");
    assert!(
        drop.contains("because other objects depend on it"),
        "PostgreSQL refuses this too; SPG said {drop:?}"
    );

    // The view still answers, with the column it was made with.
    e.execute("SELECT id, v FROM wv")
        .expect("the view is intact");
}
