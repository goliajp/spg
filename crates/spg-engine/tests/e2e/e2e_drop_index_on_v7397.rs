//! v7.39.7 — `DROP INDEX i ON t`, which is how MySQL drops an index.
//!
//! SPG had the two dialects exactly backwards on the MySQL wire.
//! Measured against MySQL 9.7.2 and the published 7.39.6 image, same
//! statements, same client:
//!
//! ```text
//!                                   MySQL 9.7.2       spg 7.39.6
//!   DROP INDEX ix ON ci                works       syntax error 1064
//!   ALTER TABLE ci DROP INDEX ix       works            works
//!   DROP INDEX ix                   syntax error       accepted
//! ```
//!
//! So a migration that drops an index failed against the drop-in and
//! not against the thing it replaces — and a script written against
//! SPG would fail on the MySQL it claims to be.
//!
//! MySQL's edges, all measured rather than assumed:
//!
//! ```text
//!   DROP INDEX ix ON c2          (index is on c1)  1091 Can't DROP 'ix'
//!   DROP INDEX nosuch ON c1                        1091 Can't DROP 'nosuch'
//!   DROP INDEX IF EXISTS ix ON c1                  1064 syntax error
//!   DROP INDEX ix ON nosuchtable                   1146 Table … doesn't exist
//! ```
//!
//! PostgreSQL has no `ON` clause here at all: its index names live in
//! the schema, not in a table. So the PostgreSQL dialect keeps the bare
//! form and the MySQL one requires `ON`, each refusing what its engine
//! refuses.

use spg_engine::{Engine, QueryResult};

fn pg() -> Engine {
    let mut e = Engine::new();
    seed(&mut e);
    e
}

fn mysql() -> Engine {
    let mut e = Engine::new();
    e.set_mysql_dialect(true);
    seed(&mut e);
    e
}

fn seed(e: &mut Engine) {
    e.execute("CREATE TABLE c1 (id INT PRIMARY KEY, a TEXT)")
        .unwrap();
    e.execute("CREATE TABLE c2 (id INT PRIMARY KEY, a TEXT)")
        .unwrap();
    e.execute("CREATE INDEX ix ON c1 (a)").unwrap();
}

fn index_exists(e: &mut Engine, table: &str, name: &str) -> bool {
    let QueryResult::Rows { rows, .. } = e
        .execute(&format!(
            "SELECT count(*) FROM pg_indexes WHERE tablename = '{table}' AND indexname = '{name}'"
        ))
        .unwrap()
    else {
        panic!("expected Rows");
    };
    match &rows[0].values[0] {
        spg_storage::Value::BigInt(n) => *n > 0,
        spg_storage::Value::Int(n) => *n > 0,
        other => panic!("{other:?}"),
    }
}

#[test]
fn mysql_drops_by_its_own_spelling() {
    let mut e = mysql();
    assert!(index_exists(&mut e, "c1", "ix"));
    e.execute("DROP INDEX ix ON c1")
        .expect("MySQL's own DROP INDEX spelling");
    assert!(
        !index_exists(&mut e, "c1", "ix"),
        "the index is still there"
    );
}

#[test]
fn mysql_refuses_the_postgresql_spelling() {
    let mut e = mysql();
    // The wording is the dialect's own: every `expected …` this parser
    // raises is rewritten on the way out to the one syntax-error
    // sentence the engine being imitated prints, so what is pinned here
    // is that it IS a syntax error and that nothing was dropped.
    let err = e.execute("DROP INDEX ix").unwrap_err();
    assert!(
        matches!(err, spg_engine::EngineError::Parse(_)),
        "expected a syntax error, got {err:?}"
    );
    assert!(
        index_exists(&mut e, "c1", "ix"),
        "a refused statement must not have dropped anything"
    );
}

#[test]
fn mysql_refuses_if_exists() {
    // MySQL answers 1064 for `DROP INDEX IF EXISTS ix ON c1`.
    let mut e = mysql();
    assert!(e.execute("DROP INDEX IF EXISTS ix ON c1").is_err());
    assert!(index_exists(&mut e, "c1", "ix"));
}

#[test]
fn the_index_must_be_on_the_named_table() {
    // MySQL: `Can't DROP 'ix'` — the index exists, just not there.
    let mut e = mysql();
    assert!(e.execute("DROP INDEX ix ON c2").is_err());
    assert!(
        index_exists(&mut e, "c1", "ix"),
        "the index on the OTHER table must survive"
    );
}

#[test]
fn a_missing_table_is_a_missing_table() {
    // MySQL answers 1146 here, not 1091: a different error from the
    // index being absent, so the two must not collapse.
    let mut e = mysql();
    let err = e.execute("DROP INDEX ix ON nosuchtable").unwrap_err();
    let text = format!("{err:?}").to_lowercase();
    assert!(
        text.contains("nosuchtable"),
        "the error must name the table, got {err:?}"
    );
}

#[test]
fn postgresql_keeps_the_bare_form() {
    let mut e = pg();
    e.execute("DROP INDEX ix").expect("PostgreSQL's spelling");
    assert!(!index_exists(&mut e, "c1", "ix"));
}

#[test]
fn postgresql_if_exists_is_still_idempotent() {
    let mut e = pg();
    e.execute("DROP INDEX IF EXISTS nosuch")
        .expect("idempotent");
}

#[test]
fn the_alter_spelling_scopes_to_its_own_table() {
    // `ALTER TABLE c2 DROP INDEX ix` names c2, and `ix` is on c1.
    for mut e in [pg(), mysql()] {
        assert!(e.execute("ALTER TABLE c2 DROP INDEX ix").is_err());
        assert!(
            index_exists(&mut e, "c1", "ix"),
            "the index on c1 must survive an ALTER naming c2"
        );
        e.execute("ALTER TABLE c1 DROP INDEX ix")
            .expect("the table that actually holds it");
        assert!(!index_exists(&mut e, "c1", "ix"));
    }
}
