//! Round 700 — the sweep's fourth batch, and the one real acceptance gap
//! in it: `CREATE VIEW` over a table that does not exist.
//!
//! Ten ALTER/DDL shapes measured against PG18. Seven already agreed —
//! DROP CONSTRAINT, RENAME COLUMN, ALTER COLUMN and DROP COLUMN over a
//! missing column, GRANT and CREATE POLICY over a missing relation, and a
//! duplicate constraint name. Three did not:
//!
//!   * `CREATE VIEW v AS SELECT * FROM nosuch` REPORTED SUCCESS. It left a
//!     view listed in `pg_views` that every SELECT against fails, and that
//!     a dump then carries forward. A statement that says it worked and
//!     produces a broken object is worse than one that refuses.
//!
//!   * `DROP TRIGGER nosuch ON t` said `corrupt on-disk format: trigger
//!     "nosuch" on "t" does not exist` — the banner round 698 fixed for
//!     sequences, plus SPG's own wording where PG says `for table "t"`.
//!     Round 698 wrote that its sweep found nothing else; it had swept the
//!     sequence, view and type shapes and not the trigger one. The sentence
//!     was broader than the sweep.
//!
//!   * `ALTER INDEX nosuch RENAME TO x` said `index "nosuch" does not
//!     exist`; PG says `relation "nosuch" …`, because an index is a
//!     relation there — and because the wire classifier reads the relation
//!     wording for 42P01.
//!
//! The view check is `view_output_columns`, which the CREATE OR REPLACE
//! path already ran: a `LIMIT 0` execution of the same body. It cannot
//! disagree with what the view will do, because it IS what the view will
//! do.

use spg_engine::{Engine, QueryResult};

fn err_of(e: &mut Engine, sql: &str) -> String {
    format!("{}", e.execute(sql).expect_err(&format!("PG18 refuses: {sql}")))
}

#[test]
fn round700_a_view_over_a_missing_relation_is_refused() {
    let mut e = Engine::new();
    let err = err_of(&mut e, "CREATE VIEW v700 AS SELECT * FROM nosuch700");
    assert!(err.contains("relation \"nosuch700\" does not exist"), "{err}");
    // And nothing was left behind: the whole point is that the catalog does
    // not gain an object that cannot be read.
    let views = match e.execute("SELECT viewname FROM pg_views").unwrap() {
        QueryResult::Rows { rows, .. } => rows
            .iter()
            .map(|r| spg_engine::eval::value_to_text(&r.values[0]))
            .collect::<Vec<_>>(),
        other => panic!("{other:?}"),
    };
    assert!(!views.iter().any(|v| v == "v700"), "{views:?}");
}

/// A missing COLUMN in the body is caught by the same probe, and so is a
/// body that is fine — the check must not cost the working path.
#[test]
fn round700_the_view_body_check_is_the_body_itself() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE src700(i INT)").unwrap();
    assert!(
        err_of(&mut e, "CREATE VIEW v700b AS SELECT nosuchcol FROM src700")
            .contains("nosuchcol"),
    );
    e.execute("CREATE VIEW v700c AS SELECT i FROM src700").unwrap();
    e.execute("INSERT INTO src700 VALUES (1)").unwrap();
    let n = match e.execute("SELECT count(*) FROM v700c").unwrap() {
        QueryResult::Rows { rows, .. } => spg_engine::eval::value_to_text(&rows[0].values[0]),
        other => panic!("{other:?}"),
    };
    assert_eq!(n, "1");
    // A view over another view still resolves.
    e.execute("CREATE VIEW v700d AS SELECT i FROM v700c").unwrap();
    // And a CTE body, which resolves nothing from the catalog, is fine.
    e.execute("CREATE VIEW v700e AS WITH c AS (SELECT 1 AS x) SELECT x FROM c")
        .unwrap();
}

#[test]
fn round700_drop_trigger_says_what_pg_says_without_a_corruption_banner() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t700(i INT)").unwrap();
    let err = err_of(&mut e, "DROP TRIGGER nosuch700 ON t700");
    assert!(
        err.contains("trigger \"nosuch700\" for table \"t700\" does not exist"),
        "{err}"
    );
    assert!(!err.contains("corrupt on-disk format"), "{err}");
    // IF EXISTS still says nothing.
    e.execute("DROP TRIGGER IF EXISTS nosuch700 ON t700").unwrap();
}

#[test]
fn round700_alter_index_rename_names_a_relation() {
    let mut e = Engine::new();
    let err = err_of(&mut e, "ALTER INDEX nosuch700 RENAME TO x700");
    assert!(err.contains("relation \"nosuch700\" does not exist"), "{err}");
    e.execute("ALTER INDEX IF EXISTS nosuch700 RENAME TO x700")
        .unwrap();
}

/// The seven that already agreed, pinned together so a later change has to
/// answer for the whole batch rather than one line of it.
#[test]
fn round700_the_shapes_that_already_matched_pg18() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t700(i INT PRIMARY KEY, j INT)")
        .unwrap();
    e.execute("ALTER TABLE t700 ADD CONSTRAINT c700 CHECK (j > 0)")
        .unwrap();
    for (sql, want) in [
        (
            "ALTER TABLE t700 DROP CONSTRAINT nosuch700",
            "constraint \"nosuch700\" of relation \"t700\" does not exist",
        ),
        (
            "ALTER TABLE t700 RENAME COLUMN nosuch700 TO x",
            "column \"nosuch700\" does not exist",
        ),
        (
            "ALTER TABLE t700 ALTER COLUMN nosuch700 SET NOT NULL",
            "column \"nosuch700\" of relation \"t700\" does not exist",
        ),
        (
            "ALTER TABLE t700 DROP COLUMN nosuch700",
            "column \"nosuch700\" of relation \"t700\" does not exist",
        ),
        ("GRANT SELECT ON nosuch700 TO postgres", "does not exist"),
        ("CREATE POLICY p700 ON nosuch700 USING (true)", "does not exist"),
        (
            "ALTER TABLE t700 ADD CONSTRAINT c700 CHECK (j > 1)",
            "constraint \"c700\" for relation \"t700\" already exists",
        ),
    ] {
        let err = err_of(&mut e, sql);
        assert!(err.contains(want), "{sql}\n  got: {err}\n  want: {want}");
    }
}
