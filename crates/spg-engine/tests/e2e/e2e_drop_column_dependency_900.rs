//! 9.0.0 (D8) — `DROP COLUMN` is refused when a view reads that column.
//!
//! PostgreSQL's dependency is COLUMN-precise. SPG's was relation-level
//! (D7), so it could not tell the two apart and allowed both — leaving
//! a view whose every SELECT then failed, made by a statement that said
//! it worked.
//!
//! Measured on PostgreSQL 18.6:
//!
//! ```text
//!   CREATE TABLE dt8(a int, b int); CREATE VIEW dv8 AS SELECT a FROM dt8;
//!   ALTER TABLE dt8 DROP COLUMN b   -- accepted: the view does not read it
//!   ALTER TABLE dt8 DROP COLUMN a
//!     ERROR:  cannot drop column a of table dt8 because other objects depend on it
//!     DETAIL:  view dv8 depends on column a of table dt8
//!     HINT:  Use DROP ... CASCADE to drop the dependent objects too.
//! ```

use spg_engine::Engine;

fn seeded() -> Engine {
    let mut e = Engine::new();
    for sql in [
        "CREATE TABLE dt8 (a int, b int, c int)",
        "CREATE VIEW dv8 AS SELECT a FROM dt8",
        "CREATE VIEW dv9 AS SELECT c FROM dt8 WHERE b > 0",
    ] {
        e.execute(sql)
            .unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    }
    e
}

#[test]
fn a_column_a_view_reads_cannot_be_dropped() {
    let mut e = seeded();
    let err = e
        .execute("ALTER TABLE dt8 DROP COLUMN a")
        .expect_err("the view reads a");
    let msg = format!("{err}");
    assert!(
        msg.contains("cannot drop column a of table dt8 because other objects depend on it"),
        "{msg}"
    );
    assert!(
        msg.contains("DETAIL:  view dv8 depends on column a of table dt8"),
        "{msg}"
    );
    assert!(msg.contains("Use DROP ... CASCADE"), "{msg}");
}

#[test]
fn a_column_read_only_in_a_predicate_counts() {
    let mut e = seeded();
    let err = e
        .execute("ALTER TABLE dt8 DROP COLUMN b")
        .expect_err("dv9's WHERE reads b");
    assert!(
        format!("{err}").contains("view dv9 depends on column b"),
        "{err:?}"
    );
}

#[test]
fn a_column_no_view_reads_still_drops() {
    // The half PostgreSQL allows, and the reason a relation-level check
    // is not the answer.
    let mut e = Engine::new();
    e.execute("CREATE TABLE d2 (a int, unused int)").expect("t");
    e.execute("CREATE VIEW d2v AS SELECT a FROM d2").expect("v");
    e.execute("ALTER TABLE d2 DROP COLUMN unused")
        .expect("no view reads it");
}

#[test]
fn a_star_over_a_derived_table_is_asked_rather_than_assumed() {
    // A `*` that survives D9's expansion stands over a subquery, and
    // the subquery's own select list says which columns it reads.
    // Measured on PG 18.6: `DROP COLUMN c` is allowed under
    // `SELECT * FROM (SELECT a, b FROM sq) q`, and `b` is refused.
    let mut e = Engine::new();
    e.execute("CREATE TABLE sq (a int, b int, c int)")
        .expect("t");
    e.execute("CREATE VIEW sv AS SELECT * FROM (SELECT a, b FROM sq) q")
        .expect("v");
    e.execute("ALTER TABLE sq DROP COLUMN c")
        .expect("the subquery does not read c");
    let err = e
        .execute("ALTER TABLE sq DROP COLUMN b")
        .expect_err("the subquery reads b");
    assert!(
        format!("{err}").contains("view sv depends on column b of table sq"),
        "{err:?}"
    );
}

#[test]
fn a_star_over_a_plain_relation_reads_all_of_it() {
    // The expansion bails when the FROM holds something the catalog
    // cannot enumerate, and the `*` then covers the plain relation
    // beside it. PostgreSQL expands its own `*` and refuses too —
    // measured on 18.6, `DROP COLUMN b` under
    // `SELECT * FROM gt, generate_series(1,2) g` is refused.
    let mut e = Engine::new();
    e.execute("CREATE TABLE gt (a int, b int)").expect("t");
    e.execute("CREATE VIEW gv AS SELECT * FROM gt, generate_series(1, 2) g")
        .expect("v");
    let err = e
        .execute("ALTER TABLE gt DROP COLUMN b")
        .expect_err("the star covers gt");
    assert!(
        format!("{err}").contains("view gv depends on column b of table gt"),
        "{err:?}"
    );
}

#[test]
fn the_view_still_answers_after_the_allowed_drop() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE d3 (a int, unused int)").expect("t");
    e.execute("INSERT INTO d3 VALUES (7, 1)").expect("insert");
    e.execute("CREATE VIEW d3v AS SELECT a FROM d3").expect("v");
    e.execute("ALTER TABLE d3 DROP COLUMN unused")
        .expect("drop");
    let spg_engine::QueryResult::Rows { rows, .. } =
        e.execute("SELECT a FROM d3v").expect("select")
    else {
        panic!("expected rows")
    };
    assert_eq!(
        spg_engine::eval::value_to_text(&rows[0].values[0]),
        "7",
        "the view broke on a drop that was allowed"
    );
}
