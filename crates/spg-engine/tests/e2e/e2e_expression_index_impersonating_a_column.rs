//! 8.0.3 — an expression index answering a question about its COLUMN.
//!
//! `CREATE UNIQUE INDEX ON t (LOWER(a))` records `column_position` = a's
//! position and an `expression`. Two index-selection filters on write
//! paths matched on the position and never asked about the expression,
//! so a tree keyed by `lower(a)` answered questions about `a`, probed
//! with the raw value.
//!
//! Found by sweeping the shapes around sentori's §3.22, which named the
//! expression arbiter as unsupported. Unsupported was not what it was.
//!
//! The foreign key is the worse of the two, because it is INVERTED
//! rather than merely wrong. Measured against PostgreSQL 18.6, parent
//! holding `'A'`:
//!
//! ```text
//!                    PG 18.6     SPG 8.0.2
//!   child 'A'        accepted    REFUSED   Key (k)=(A) is not present
//!   child 'a'        refused     ACCEPTED
//! ```
//!
//! A legitimate child rejected and an orphan admitted. Both tables end
//! with one row, so a count agrees while the surviving row is the wrong
//! one — which is why nothing here had caught it.
//!
//! And the arbiter, same cause, one row present (`'X'`):
//!
//! ```text
//!   INSERT ('x') ON CONFLICT (a) DO NOTHING
//!     PG 18.6   ERROR: there is no unique or exclusion constraint
//!               matching the ON CONFLICT specification
//!     SPG       INSERT 0 0        -- and the row is gone
//! ```
//!
//! `SELECT count(*) FROM t WHERE a = 'x'` is 0 on both engines, so
//! nothing in the data said those two rows should collide.
//!
//! On the arbiter this file pins the PROPERTY rather than PostgreSQL's
//! message, and says so: PostgreSQL refuses at plan time because the
//! target matches no unique constraint, and SPG deliberately accepts
//! targets nothing enforces (recorded in `constraints.rs`, and mailrs
//! depends on it). What must never happen again is the statement
//! reporting success while the row disappears.

use spg_engine::{Engine, QueryResult};

fn run(e: &mut Engine, sql: &str) -> Result<QueryResult, spg_engine::EngineError> {
    e.execute(sql)
}

fn setup(sqls: &[&str]) -> Engine {
    let mut e = Engine::new();
    for sql in sqls {
        e.execute(sql)
            .unwrap_or_else(|x| panic!("setup {sql:?}: {x:?}"));
    }
    e
}

fn count(e: &mut Engine, sql: &str) -> usize {
    match e.execute(sql).unwrap_or_else(|x| panic!("{sql}: {x:?}")) {
        QueryResult::Rows { rows, .. } => rows.len(),
        other => panic!("{sql}: expected Rows, got {other:?}"),
    }
}

/// The expression index is created BEFORE the plain unique one, so it
/// comes first in the index list and is the one a position-only filter
/// reaches. That ordering is the whole reproduction.
fn fk_tables() -> Engine {
    setup(&[
        "CREATE TABLE gp (k text)",
        "CREATE INDEX gp_lower_k ON gp (LOWER(k))",
        "CREATE UNIQUE INDEX gp_k ON gp (k)",
        "INSERT INTO gp VALUES ('A')",
        "CREATE TABLE gc (id int, k text REFERENCES gp(k))",
    ])
}

#[test]
fn a_child_whose_parent_exists_is_accepted() {
    let mut e = fk_tables();
    run(&mut e, "INSERT INTO gc VALUES (1,'A')")
        .expect("parent 'A' is present; PostgreSQL 18.6 accepts this row");
    assert_eq!(count(&mut e, "SELECT id FROM gc"), 1);
}

#[test]
fn a_child_whose_parent_does_not_exist_is_refused() {
    let mut e = fk_tables();
    let err = run(&mut e, "INSERT INTO gc VALUES (2,'a')")
        .expect_err("parent 'a' is absent; PostgreSQL 18.6 refuses this row");
    let text = alloc_string(&err);
    assert!(
        text.contains("foreign key"),
        "the refusal must name the foreign key, got {text:?}"
    );
    assert_eq!(count(&mut e, "SELECT id FROM gc"), 0);
}

/// The pair together, in one engine, because the defect swapped them:
/// each half passing alone would not have shown it.
#[test]
fn the_two_children_land_on_the_right_sides() {
    let mut e = fk_tables();
    assert!(run(&mut e, "INSERT INTO gc VALUES (1,'A')").is_ok());
    assert!(run(&mut e, "INSERT INTO gc VALUES (2,'a')").is_err());
    assert_eq!(count(&mut e, "SELECT id FROM gc WHERE k = 'A'"), 1);
    assert_eq!(count(&mut e, "SELECT id FROM gc WHERE k = 'a'"), 0);
}

#[test]
fn on_conflict_on_a_column_does_not_silently_drop_the_row() {
    let mut e = setup(&[
        "CREATE TABLE q2 (id int, a text)",
        "CREATE UNIQUE INDEX q2_lower_a ON q2 (LOWER(a))",
        "INSERT INTO q2 VALUES (1,'X')",
    ]);
    // Nothing makes `a` itself unique, and no row has a = 'x'.
    assert_eq!(count(&mut e, "SELECT id FROM q2 WHERE a = 'x'"), 0);
    let before = count(&mut e, "SELECT id FROM q2");
    let r = run(
        &mut e,
        "INSERT INTO q2 VALUES (2,'x') ON CONFLICT (a) DO NOTHING",
    );
    let after = count(&mut e, "SELECT id FROM q2");
    match r {
        // Refused: the row is not there, and the caller was told.
        Err(_) => assert_eq!(after, before, "a refused INSERT must not change the table"),
        // Accepted: then the row must BE there. What must never happen
        // is success with the row gone, which is what 8.0.2 did.
        Ok(_) => assert_eq!(
            after,
            before + 1,
            "the statement reported success and the row is not in the table"
        ),
    }
}

/// The adjacent shapes, which already answered and must keep answering.
/// If one of these breaks, the filter reached further than the defect.
#[test]
fn the_adjacent_shapes_that_already_answered_still_do() {
    // A plain unique index still arbitrates.
    let mut e = setup(&[
        "CREATE TABLE p1 (id int, a text)",
        "CREATE UNIQUE INDEX p1_a ON p1 (a)",
        "INSERT INTO p1 VALUES (1,'x')",
    ]);
    run(
        &mut e,
        "INSERT INTO p1 VALUES (2,'x') ON CONFLICT (a) DO NOTHING",
    )
    .expect("a plain unique index is still an arbiter");
    assert_eq!(count(&mut e, "SELECT id FROM p1"), 1);

    // A plain FK still enforces, both ways.
    let mut f = setup(&[
        "CREATE TABLE fp2 (k text)",
        "CREATE UNIQUE INDEX fp2_k ON fp2 (k)",
        "INSERT INTO fp2 VALUES ('A')",
        "CREATE TABLE fc2 (id int, k text REFERENCES fp2(k))",
    ]);
    run(&mut f, "INSERT INTO fc2 VALUES (1,'A')").expect("present parent");
    assert!(run(&mut f, "INSERT INTO fc2 VALUES (2,'a')").is_err());
    assert_eq!(count(&mut f, "SELECT id FROM fc2"), 1);
}

fn alloc_string(e: &spg_engine::EngineError) -> String {
    format!("{e:?}")
}
