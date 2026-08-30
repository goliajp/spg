//! v7.39.3 — whether `SET NAMES … COLLATE …` reaches a bare literal.
//!
//! `SET NAMES utf8mb4 COLLATE utf8mb4_bin` is in the first packet of a
//! MySQL client that wants byte comparison. SPG carried that collation
//! as far as PADDING and no further: the decision to FOLD case read the
//! DIALECT — "this is a MySQL session, therefore fold" — so two
//! literals still compared case-insensitively under it. Measured
//! against MySQL 9.7.2:
//!
//!     SET NAMES utf8mb4 COLLATE utf8mb4_bin
//!     SELECT 'AB' = 'ab'                        MySQL 0    SPG 1
//!     SELECT 'a' < 'B'                          MySQL 0    SPG 1
//!     SELECT 'AB' COLLATE utf8mb4_general_ci
//!            = 'ab'                             MySQL 1    SPG 0
//!
//! The third one is the other direction, and it came out of fixing the
//! first two: an explicit `COLLATE` outranks the session (MySQL's
//! coercibility rules put it highest), and SPG's parser had been
//! DROPPING an explicit `_ci` name in a MySQL session on the reasoning
//! that the dialect folds anyway — true until the fold started reading
//! the session.
//!
//! One question had three owners here (`text_compare_of`,
//! `mysql_text_fold_applies`, `compare_is_case_insensitive`); the first
//! two are one now, and the divergence between them is exactly what
//! made the first fix look as though it had not landed.

use spg_engine::{Engine, QueryResult};

fn mysql() -> Engine {
    let mut e = Engine::new();
    e.set_mysql_dialect(true);
    e
}

fn one(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql) {
        Ok(QueryResult::Rows { rows, .. }) => {
            assert_eq!(rows.len(), 1, "{sql}");
            spg_engine::eval::value_to_text(&rows[0].values[0])
        }
        other => panic!("{sql}: {other:?}"),
    }
}

#[test]
fn a_binary_session_collation_reaches_two_literals() {
    let mut e = mysql();
    assert_eq!(one(&mut e, "SELECT 'AB' = 'ab'"), "true");
    e.execute("SET NAMES utf8mb4 COLLATE utf8mb4_bin")
        .expect("set names");
    assert_eq!(
        one(&mut e, "SELECT @@collation_connection"),
        "utf8mb4_bin",
        "the session has to have taken it for the rest to mean anything"
    );
    // MySQL 9.7.2: 0.
    assert_eq!(one(&mut e, "SELECT 'AB' = 'ab'"), "false");
    // And ORDERING follows: 'B' is 0x42, 'a' is 0x61.
    assert_eq!(one(&mut e, "SELECT 'a' < 'B'"), "false");
    assert_eq!(one(&mut e, "SELECT 'B' < 'a'"), "true");
    // Back again — the session is read live, not sampled once.
    e.execute("SET NAMES utf8mb4").expect("set names");
    assert_eq!(one(&mut e, "SELECT 'AB' = 'ab'"), "true");
}

#[test]
fn an_explicit_collate_outranks_the_session() {
    let mut e = mysql();
    e.execute("SET NAMES utf8mb4 COLLATE utf8mb4_bin")
        .expect("set names");
    // MySQL 9.7.2: 1. The clause is what the session is not.
    assert_eq!(
        one(&mut e, "SELECT 'AB' COLLATE utf8mb4_general_ci = 'ab'"),
        "true"
    );
    assert_eq!(
        one(&mut e, "SELECT 'AB' COLLATE utf8mb4_0900_ai_ci = 'ab'"),
        "true"
    );
    // The other side carries it just as well.
    assert_eq!(
        one(&mut e, "SELECT 'AB' = 'ab' COLLATE utf8mb4_general_ci"),
        "true"
    );
    // And the reverse still holds under a folding session.
    e.execute("SET NAMES utf8mb4").expect("set names");
    assert_eq!(
        one(&mut e, "SELECT 'AB' COLLATE utf8mb4_bin = 'ab'"),
        "false"
    );
}

/// A `_cs` name is as case-sensitive as a `_bin` one — measured on
/// MySQL 9.7.2, `_utf8mb4'AB' COLLATE utf8mb4_0900_as_cs = _utf8mb4'ab'`
/// is 0 — and only `_bin` was being recognised.
#[test]
fn a_case_sensitive_name_does_not_fold_either() {
    let mut e = mysql();
    assert_eq!(
        one(&mut e, "SELECT 'AB' COLLATE utf8mb4_0900_as_cs = 'ab'"),
        "false"
    );
}

/// The control: a column that declares its own collation still beats
/// the session, and an ordinary MySQL session still folds by default.
/// If either of these moved, the fix reached further than its subject.
#[test]
fn a_column_and_the_default_are_where_they_were() {
    let mut e = mysql();
    e.execute("CREATE TABLE sc (a TEXT, b TEXT COLLATE utf8mb4_bin)")
        .expect("ddl");
    e.execute("INSERT INTO sc VALUES ('Foo', 'Foo')")
        .expect("insert");
    // Default session: the plain column folds, the byte-wise one does not.
    assert_eq!(one(&mut e, "SELECT count(*) FROM sc WHERE a = 'foo'"), "1");
    assert_eq!(one(&mut e, "SELECT count(*) FROM sc WHERE b = 'foo'"), "0");
    // Under a byte-wise session the plain column stops folding too,
    // which is what MySQL does with a column that declared nothing.
    e.execute("SET NAMES utf8mb4 COLLATE utf8mb4_bin")
        .expect("set names");
    assert_eq!(one(&mut e, "SELECT count(*) FROM sc WHERE b = 'foo'"), "0");
    assert_eq!(one(&mut e, "SELECT count(*) FROM sc WHERE b = 'Foo'"), "1");
}

/// A PostgreSQL session is untouched: it has no session collation to
/// read, and `'a' = 'A'` is false there whatever a MySQL client did.
#[test]
fn a_postgres_session_is_unaffected() {
    let mut e = Engine::new();
    assert_eq!(one(&mut e, "SELECT 'AB' = 'ab'"), "false");
    assert_eq!(one(&mut e, "SELECT 'a' < 'B'"), "false");
}
