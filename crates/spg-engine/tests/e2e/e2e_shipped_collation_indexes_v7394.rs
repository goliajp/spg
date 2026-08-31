//! v7.39.4 — two indexes that answered from the wrong keyspace once the
//! database named a collation, which the published image always does.
//!
//! `docker image inspect goliakk/spg:7.39.3` carries `LANG=en_US.utf8`,
//! and the server says so on the way up:
//!
//!     spg-server: database collation "en_US.utf8"
//!
//! Every record of the sqllogictest corpus, meanwhile, had only ever run
//! on a catalog that orders by BYTES — where neither of these defects
//! can exist. They were found by running that corpus a second time under
//! the collation above, and both are silent: one ADMITS A ROW that
//! violates a declared constraint, the other returns the empty set.
//!
//! The pins here set the collation themselves rather than inheriting a
//! harness default, because a default is exactly what hid them.

use spg_engine::{Engine, QueryResult};

/// A database collated the way the image collates it.
fn shipped() -> Engine {
    let mut e = Engine::new();
    e.set_database_collation("en_US.utf8")
        .expect("the shipped collation must install on an empty database");
    e
}

fn run(e: &mut Engine, sql: &str) {
    e.execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"));
}

fn rows(e: &mut Engine, sql: &str) -> Vec<String> {
    match e.execute(sql) {
        Ok(QueryResult::Rows { rows, .. }) => rows
            .iter()
            .map(|r| spg_engine::eval::value_to_text(&r.values[0]))
            .collect(),
        other => panic!("{sql}: {other:?}"),
    }
}

/// A `UNIQUE` text column must refuse the second `'a'`.
///
/// It did not. The constraint descends the column's B-tree when one
/// discriminates; a locale-collated tree is keyed by ICU sort keys and
/// is empty until a refresh fills it, so a lookup by the RAW value
/// answered "no locators" — read as the most selective candidate, taken,
/// and the conflicting row never compared. The table then held two rows
/// both `'a'` in a column declared UNIQUE.
#[test]
fn a_unique_text_column_still_refuses_a_duplicate() {
    let mut e = shipped();
    run(
        &mut e,
        "CREATE TABLE u (id INT NOT NULL, code TEXT NOT NULL UNIQUE)",
    );
    run(&mut e, "INSERT INTO u VALUES (1, 'a')");
    let second = e.execute("INSERT INTO u VALUES (2, 'a')");
    assert!(
        second.is_err(),
        "the duplicate was ACCEPTED on a database that names a collation: {second:?}"
    );
    assert_eq!(rows(&mut e, "SELECT code FROM u"), vec!["a".to_string()]);
}

/// The same table on a byte-ordering database — the configuration every
/// run of this suite has had, and where the defect cannot exist. It is
/// here to say what the pin above is worth: this one passed throughout.
#[test]
fn a_unique_text_column_refuses_a_duplicate_on_a_byte_ordering_database() {
    let mut e = Engine::new();
    run(
        &mut e,
        "CREATE TABLE u (id INT NOT NULL, code TEXT NOT NULL UNIQUE)",
    );
    run(&mut e, "INSERT INTO u VALUES (1, 'a')");
    assert!(e.execute("INSERT INTO u VALUES (2, 'a')").is_err());
}

/// A GIN keys on lexemes the engine derives, not on the column's value,
/// so a locale collation has nothing to say about it. Asked only "is
/// this a text column on a collated database", every GIN was routed into
/// the ICU path and answered nothing.
#[test]
fn a_fulltext_index_still_answers_on_a_collated_database() {
    // No MySQL dialect, matching the corpus file this came from. It
    // matters: a MySQL text column carries `Collation::CaseInsensitive`,
    // which the classification returns `None` for before it ever looks
    // at the index — so the defect is unreachable from a MySQL session
    // and a pin written there passes with the fix removed. It did;
    // that is how this note came to be here.
    let mut e = shipped();
    run(
        &mut e,
        "CREATE TABLE posts (id INT NOT NULL, body TEXT, FULLTEXT KEY body_ft (body))",
    );
    run(&mut e, "INSERT INTO posts VALUES (1, 'Quick brown fox')");
    run(&mut e, "INSERT INTO posts VALUES (2, 'Lazy DOG sleeping')");
    assert_eq!(
        rows(
            &mut e,
            "SELECT id FROM posts WHERE MATCH(body) AGAINST ('brown') ORDER BY id"
        ),
        vec!["1".to_string()],
        "the fulltext index answered nothing on a database that names a collation"
    );
}

/// The trigram GIN, which is the same defect on a plain PostgreSQL
/// index rather than a MySQL-shaped one.
///
/// The row that matters is the one inserted AFTER the index exists.
/// Rows already present when `CREATE INDEX` runs are seeded by a path
/// that works either way; it is the incremental maintenance that took
/// the supplied-key branch and dropped the new row's trigrams on the
/// floor. A first version of this pin inserted both rows first and
/// passed with the fix removed.
#[test]
fn a_trigram_index_still_answers_on_a_collated_database() {
    let mut e = shipped();
    run(&mut e, "CREATE TABLE t (id INT NOT NULL, s TEXT)");
    run(&mut e, "INSERT INTO t VALUES (1, 'alphabet soup')");
    run(
        &mut e,
        "CREATE INDEX t_s_trgm ON t USING gin (s gin_trgm_ops)",
    );
    run(&mut e, "INSERT INTO t VALUES (2, 'alphabet again')");
    let mut got = rows(&mut e, "SELECT id FROM t WHERE s LIKE '%phabet%'");
    got.sort();
    assert_eq!(
        got,
        vec!["1".to_string(), "2".to_string()],
        "the row inserted after the index was not indexed on a database \
         that names a collation"
    );
}
