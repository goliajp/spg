//! v7.40.11 — Describe answered a shape for a statement that cannot run.
//!
//! Reported against 7.40.9 (§3.4). `describe_prepared` returns a shape
//! and has no error channel, so a client that prepares a statement
//! against a table it misspelled is told the Parse succeeded, and only
//! finds out one round trip later at Execute. A tool that never
//! executes — a schema browser, a query builder, sqlx's `--check` — is
//! never corrected at all.
//!
//! The second row is the worse one: it does not merely stay silent, it
//! invents a column and gives it a type.
//!
//! ```text
//!                                    PG 18.6                    SPG 7.40.10
//!   SELECT * FROM no_such_table      relation … does not exist  no result, no columns
//!   SELECT nosuchcol FROM pg_class   column … does not exist    nosuchcol|text
//! ```
//!
//! The boundary below is measured on PostgreSQL 18.6 with `PREPARE`,
//! which is where its parse analysis runs — the same point the extended
//! protocol raises it at Parse:
//!
//! ```text
//!   SELECT * FROM no_such_table                          relation … does not exist
//!   SELECT nosuchcol FROM pg_class                       column … does not exist
//!   SELECT nosuchcol FROM dr                             column … does not exist
//!   SELECT * FROM dr WHERE nosuchcol = 1                 column … does not exist
//!   SELECT * FROM (SELECT * FROM no_such_table) z        relation … does not exist
//!   SELECT * FROM dr JOIN no_such_table t ON true        relation … does not exist
//!   WITH c AS (…) SELECT * FROM c JOIN no_such_table …   relation … does not exist
//!   SELECT a FROM dr                                     PREPARE
//!   SELECT a FROM drv          (a view)                  PREPARE
//!   SELECT last_value FROM drs (a sequence)              PREPARE
//!   WITH c AS (SELECT 1 x) SELECT x FROM c               PREPARE
//!   SELECT * FROM generate_series(1,3)                   PREPARE
//!   SELECT * FROM pg_stat_activity                       PREPARE
//!   SELECT * FROM information_schema.tables              PREPARE
//! ```
//!
//! The accepted half is the load-bearing half of this test. A relation
//! name reaches its rows by six different routes here — a table, a
//! view, a sequence, a CTE, a synthesised `pg_catalog` view, a stat
//! view built inside its own `exec_*` — and a check that knows about
//! fewer than all six turns a working query into a Parse error, which
//! is far worse than the defect it fixes.

use spg_engine::{Engine, EngineError};

fn base() -> Engine {
    let mut eng = Engine::new();
    for sql in [
        "CREATE TABLE dr (a INT, b TEXT)",
        "INSERT INTO dr VALUES (1,'x')",
        "CREATE VIEW drv AS SELECT a FROM dr",
        "CREATE SEQUENCE drs",
    ] {
        eng.execute(sql)
            .unwrap_or_else(|e| panic!("setup {sql:?}: {e:?}"));
    }
    eng
}

fn prepare_err(eng: &mut Engine, n: usize, select: &str) -> String {
    let sql = format!("PREPARE dp{n} AS {select}");
    match eng.execute(&sql) {
        Err(EngineError::Unsupported(m)) => m,
        Err(other) => format!("{other}"),
        Ok(ok) => panic!("{select:?}: accepted ({ok:?}), PG 18.6 refuses it"),
    }
}

fn prepare_ok(eng: &mut Engine, n: usize, select: &str) {
    let sql = format!("PREPARE dq{n} AS {select}");
    eng.execute(&sql)
        .unwrap_or_else(|e| panic!("{select:?}: PG 18.6 prepares this, we refused it: {e:?}"));
}

#[test]
fn a_missing_relation_is_refused_at_prepare() {
    let mut eng = base();
    let msg = prepare_err(&mut eng, 1, "SELECT * FROM no_such_table");
    assert!(
        msg.contains("relation \"no_such_table\" does not exist"),
        "{msg}"
    );
}

#[test]
fn a_missing_column_on_a_catalog_view_is_refused() {
    let mut eng = base();
    let msg = prepare_err(&mut eng, 2, "SELECT nosuchcol FROM pg_class");
    assert!(msg.contains("column \"nosuchcol\" does not exist"), "{msg}");
}

/// The reported shapes are only two of them. Every position PG runs its
/// parse analysis over is checked, because fixing the two that were
/// filed and leaving the rest is how this comes back.
#[test]
fn every_position_pg_refuses() {
    let mut eng = base();
    let relation: &[&str] = &[
        "SELECT * FROM no_such_table",
        "SELECT * FROM (SELECT * FROM no_such_table) z",
        "SELECT * FROM dr JOIN no_such_table t ON true",
        "WITH c AS (SELECT 1 AS x) SELECT * FROM c JOIN no_such_table t ON true",
        "SELECT * FROM dr WHERE a IN (SELECT a FROM no_such_table)",
    ];
    for (i, sql) in relation.iter().enumerate() {
        let msg = prepare_err(&mut eng, 100 + i, sql);
        assert!(
            msg.contains("relation \"no_such_table\" does not exist"),
            "{sql:?}: {msg}"
        );
    }
    let column: &[&str] = &[
        "SELECT nosuchcol FROM dr",
        "SELECT * FROM dr WHERE nosuchcol = 1",
        "SELECT nosuchcol FROM pg_class",
    ];
    for (i, sql) in column.iter().enumerate() {
        let msg = prepare_err(&mut eng, 200 + i, sql);
        assert!(
            msg.contains("column \"nosuchcol\" does not exist"),
            "{sql:?}: {msg}"
        );
    }
}

/// The half that must not move. Six routes from a name in FROM to its
/// rows; a check that knows five of them breaks the sixth.
#[test]
fn every_route_a_relation_name_can_take_still_prepares() {
    let mut eng = base();
    let ok: &[&str] = &[
        "SELECT a FROM dr",
        "SELECT a FROM dr d WHERE d.a = 1",
        "SELECT a FROM drv",
        "SELECT last_value FROM drs",
        "WITH c AS (SELECT 1 AS x) SELECT x FROM c",
        "WITH c AS (SELECT a FROM dr) SELECT c.a FROM c JOIN dr ON dr.a = c.a",
        "SELECT * FROM generate_series(1,3)",
        "SELECT * FROM unnest(ARRAY[1,2,3])",
        "SELECT * FROM pg_class",
        "SELECT relname FROM pg_class",
        "SELECT * FROM pg_catalog.pg_class",
        "SELECT * FROM information_schema.tables",
        "SELECT * FROM pg_stat_activity",
        "SELECT * FROM pg_stat_user_tables",
        "SELECT * FROM (SELECT a FROM dr) z",
        "SELECT z.a FROM (SELECT a FROM dr) z WHERE z.a = 1",
        "SELECT * FROM dr, generate_series(1,3) g",
        "SELECT ctid, a FROM dr",
        "SELECT count(*) FROM dr",
    ];
    for (i, sql) in ok.iter().enumerate() {
        prepare_ok(&mut eng, 300 + i, sql);
    }
}

/// A plain SELECT is unaffected: the check belongs to the statement's
/// analysis, and the message it produces for a missing relation is the
/// one the read path already produced.
#[test]
fn the_direct_read_path_says_the_same_thing() {
    let mut eng = base();
    let direct = match eng.execute("SELECT * FROM no_such_table") {
        Err(e) => format!("{e}"),
        Ok(ok) => panic!("accepted: {ok:?}"),
    };
    assert!(
        direct.contains("relation \"no_such_table\" does not exist"),
        "{direct}"
    );
    let prepared = prepare_err(&mut eng, 400, "SELECT * FROM no_such_table");
    assert!(
        prepared.contains("relation \"no_such_table\" does not exist"),
        "{prepared}"
    );
}

/// PostgreSQL analyses a DML statement at PREPARE too, and one of these
/// was not a Describe defect at all: `DELETE FROM dr WHERE nosuchcol =
/// 1` RAN, matched nothing because the predicate names a column that
/// does not exist, and reported success. The WHERE clause of a DELETE
/// was the one clause nothing checked.
///
/// Measured on PostgreSQL 18.6:
///
/// ```text
///   INSERT INTO no_such_table VALUES (1)     relation "no_such_table" does not exist
///   UPDATE no_such_table SET a = 1           relation "no_such_table" does not exist
///   DELETE FROM no_such_table                relation "no_such_table" does not exist
///   INSERT INTO dr (nosuchcol) VALUES (1)    column "nosuchcol" of relation "dr" …
///   UPDATE dr SET nosuchcol = 1              column "nosuchcol" of relation "dr" …
///   DELETE FROM dr WHERE nosuchcol = 1       column "nosuchcol" does not exist
///   INSERT INTO dr SELECT * FROM no_such_…   relation "no_such_table" does not exist
/// ```
#[test]
fn dml_is_analysed_the_way_pg_analyses_it() {
    let mut eng = base();
    for (sql, want) in [
        (
            "INSERT INTO no_such_table VALUES (1)",
            "relation \"no_such_table\" does not exist",
        ),
        (
            "UPDATE no_such_table SET a = 1",
            "relation \"no_such_table\" does not exist",
        ),
        (
            "DELETE FROM no_such_table",
            "relation \"no_such_table\" does not exist",
        ),
        (
            "INSERT INTO dr SELECT * FROM no_such_table",
            "relation \"no_such_table\" does not exist",
        ),
        (
            "INSERT INTO dr (nosuchcol) VALUES (1)",
            "column \"nosuchcol\" of relation \"dr\" does not exist",
        ),
        (
            "UPDATE dr SET nosuchcol = 1",
            "column \"nosuchcol\" of relation \"dr\" does not exist",
        ),
        (
            "DELETE FROM dr WHERE nosuchcol = 1",
            "column \"nosuchcol\" does not exist",
        ),
    ] {
        // Both protocols, because the wording must not depend on which
        // one carried the statement.
        let direct = match eng.execute(sql) {
            Err(e) => format!("{e}"),
            Ok(ok) => panic!("{sql:?}: ran and reported {ok:?}; PG 18.6 refuses it"),
        };
        assert!(direct.contains(want), "{sql:?} direct: {direct}");
        let prepared = match eng.execute(&format!("PREPARE dmlp AS {sql}")) {
            Err(e) => format!("{e}"),
            Ok(ok) => panic!("PREPARE {sql:?}: accepted ({ok:?}), PG 18.6 refuses it"),
        };
        assert!(prepared.contains(want), "{sql:?} prepared: {prepared}");
    }
}

/// The DML that must keep working, including the shapes whose extra
/// sources put names in scope a one-table check cannot see.
#[test]
fn ordinary_dml_still_runs() {
    let mut eng = base();
    for sql in [
        "INSERT INTO dr (a, b) VALUES (2, 'y')",
        "INSERT INTO dr VALUES (3, 'z')",
        "INSERT INTO dr (a) SELECT a FROM dr WHERE a = 1",
        "WITH c AS (SELECT a FROM dr) INSERT INTO dr (a) SELECT a FROM c",
        "UPDATE dr SET b = 'q' WHERE a = 1",
        "UPDATE dr d SET b = 'q' WHERE d.a = 1",
        "UPDATE dr SET b = s.b FROM (SELECT 1 AS a, 'k' AS b) s WHERE s.a = dr.a",
        "DELETE FROM dr WHERE a = 99",
        "DELETE FROM dr d WHERE d.a = 99",
        "DELETE FROM dr USING (SELECT 98 AS a) s WHERE s.a = dr.a",
        "DELETE FROM dr WHERE ctid IS NOT NULL AND a = 97",
        "INSERT INTO dr (a, b) VALUES (4, 'r') RETURNING a, b",
        "UPDATE dr SET b = 'p' WHERE a = 4 RETURNING a",
        "DELETE FROM dr WHERE a = 4 RETURNING b",
    ] {
        eng.execute(sql)
            .unwrap_or_else(|e| panic!("{sql:?}: PG 18.6 runs this, we refused it: {e:?}"));
    }
}
