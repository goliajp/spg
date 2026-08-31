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

/// v7.39.4 — the same session, on the SHIPPED configuration.
///
/// The pins above were written on an engine whose database collates by
/// BYTES, and they passed. The server ships with a locale collation —
/// the published image carries `LANG=en_US.utf8` and its own startup
/// line says `database collation "en_US.utf8"`. That exact spelling is
/// what these pins install, rather than a neighbouring one that happens
/// to classify the same way.
///
/// There `'a' < 'B'` is TRUE by the collator, so a session that asked
/// for `utf8mb4_bin` gets the collator's order instead of the bytes it
/// asked for. Measured against MySQL 9.7.2, which answers 0, and
/// against the published 7.39.3 image, which answers 1.
///
/// The lesson is the pin's, not the code's: a collation defect only
/// exists on a locale database, so a pin that does not share the
/// shipped configuration cannot see it. This is the third time.
#[test]
fn a_binary_session_beats_a_locale_database_collation() {
    let mut e = mysql();
    assert!(
        e.set_database_collation("en_US.utf8")
            .expect("db collation"),
        "the shipped default has to be in place for this to mean anything"
    );
    // The collator's own order, with nothing asked for: 'a' before 'B'.
    assert_eq!(one(&mut e, "SELECT 'a' < 'B'"), "true");
    e.execute("SET NAMES utf8mb4 COLLATE utf8mb4_bin")
        .expect("set names");
    // MySQL 9.7.2 under this session: 0. 'a' is 0x61, 'B' is 0x42.
    assert_eq!(one(&mut e, "SELECT 'a' < 'B'"), "false");
    assert_eq!(one(&mut e, "SELECT 'a' > 'B'"), "true");
    assert_eq!(one(&mut e, "SELECT 'B' < 'a'"), "true");
    assert_eq!(one(&mut e, "SELECT 'a' <= 'B'"), "false");
    assert_eq!(one(&mut e, "SELECT 'a' >= 'B'"), "true");
    // Equality was already right, and must stay right.
    assert_eq!(one(&mut e, "SELECT 'AB' = 'ab'"), "false");
    // An explicit COLLATE still outranks the session, on this database
    // as on the other one.
    assert_eq!(
        one(&mut e, "SELECT 'AB' COLLATE utf8mb4_general_ci = 'ab'"),
        "true"
    );
    // And leaving the byte-wise session gives the database's collator
    // back — this is the control that says the fix did not simply pin
    // everything to bytes.
    e.execute("SET NAMES utf8mb4").expect("set names");
    assert_eq!(one(&mut e, "SELECT 'a' < 'B'"), "true");
    assert_eq!(one(&mut e, "SELECT 'AB' = 'ab'"), "true");
}

/// A CONTROL, and the ablation says so: removing the fix leaves this
/// green.
///
/// `_cs` is case-sensitive but not byte-wise, so its ORDER agrees with
/// the database's collator — `'a' < 'B'` is 1 under both — and only
/// `_bin` can tell the two apart. What this pin holds is the equality
/// half, which was already right and must stay right on a locale
/// database too. Measured on MySQL 9.7.2 with both operands introduced
/// as utf8mb4: `_utf8mb4'AB' COLLATE utf8mb4_0900_as_cs = _utf8mb4'ab'`
/// is 0.
#[test]
fn a_case_sensitive_session_beats_a_locale_database_collation() {
    let mut e = mysql();
    assert!(
        e.set_database_collation("en_US.utf8")
            .expect("db collation")
    );
    e.execute("SET NAMES utf8mb4 COLLATE utf8mb4_0900_as_cs")
        .expect("set names");
    assert_eq!(one(&mut e, "SELECT 'AB' = 'ab'"), "false");
}

/// The control that keeps the fix honest for PostgreSQL: a session that
/// is not MySQL's has no session collation, so the database's collator
/// stays in charge of ordering. If this moved, the fix reached past its
/// subject.
#[test]
fn a_postgres_session_still_orders_by_the_database_collation() {
    let mut e = Engine::new();
    assert!(
        e.set_database_collation("en_US.utf8")
            .expect("db collation")
    );
    assert_eq!(one(&mut e, "SELECT 'a' < 'B'"), "true");
    assert_eq!(one(&mut e, "SELECT 'B' < 'a'"), "false");
}
