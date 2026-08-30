//! v7.39.3 — what MySQL names a column holding two adjacent literals.
//!
//! `SELECT 'a' 'b'` is ONE literal on both engines and its value is
//! `ab`. MySQL 9.7.2 names the column `a` — the FIRST segment as
//! written, not the joined value — measured:
//!
//!     SELECT 'a' 'b'       value ab    name a
//!     SELECT 'a' 'b' 'c'   value abc   name a
//!     SELECT 'a' 'b' AS x  value ab    name x
//!     SELECT 'ab'          value ab    name ab
//!
//! SPG named it `ab`, because the merge happens in the LEXER and the
//! first segment's length was not carried out of it. The ledger's two
//! candidates were "give the lexer a bypass" and "let the label go back
//! to the source text and decode it again"; the second would put a
//! second escape decoder in the tree, which is the kind of pair that
//! drifts, so the length rides out of the lexer instead.

use spg_engine::{Engine, QueryResult};

fn mysql() -> Engine {
    let mut e = Engine::new();
    e.set_mysql_dialect(true);
    e
}

fn one(e: &mut Engine, sql: &str) -> (String, String) {
    match e.execute(sql) {
        Ok(QueryResult::Rows { columns, rows }) => (
            columns[0].name.clone(),
            spg_engine::eval::value_to_text(&rows[0].values[0]),
        ),
        other => panic!("{sql}: {other:?}"),
    }
}

#[test]
fn the_label_is_the_first_segment() {
    let mut e = mysql();
    assert_eq!(one(&mut e, "SELECT 'a' 'b'"), ("a".into(), "ab".into()));
    assert_eq!(
        one(&mut e, "SELECT 'a' 'b' 'c'"),
        ("a".into(), "abc".into())
    );
    // An explicit alias still wins, and a single literal still names
    // itself — the label only changes where a merge happened.
    assert_eq!(
        one(&mut e, "SELECT 'a' 'b' AS x"),
        ("x".into(), "ab".into())
    );
    assert_eq!(one(&mut e, "SELECT 'ab'"), ("ab".into(), "ab".into()));
}

/// The segments are the DECODED ones, so a backslash escape in the
/// first segment is counted as what it became, not as what was typed.
/// This is the reason the length leaves the lexer rather than being
/// recovered from the source afterwards.
#[test]
fn an_escaped_first_segment_is_measured_after_decoding() {
    let mut e = mysql();
    // `'a\tb'` is three characters once decoded.
    let (name, value) = one(&mut e, "SELECT 'a\\tb' 'c'");
    assert_eq!(value, "a\tbc");
    assert_eq!(name, "a\tb");
}

/// A PostgreSQL session is untouched: it needs a NEWLINE between the
/// two literals to merge them at all, and it names the result
/// `?column?` either way.
#[test]
fn a_postgres_session_is_unaffected() {
    let mut e = Engine::new();
    let (name, value) = one(&mut e, "SELECT 'a'\n'b'");
    assert_eq!(value, "ab");
    assert_eq!(name, "?column?");
}
