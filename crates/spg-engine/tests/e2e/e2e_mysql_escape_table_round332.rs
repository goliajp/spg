//! read01 round 332 (V35) — the MySQL escape table is its own.
//!
//! `lex_escape_string` served both PG's `E'…'` and the MySQL dialect from
//! one table. They agree on the core escapes and differ in exactly three
//! places, measured on MariaDB 11 against PG 18.4:
//!
//! | escape | PG `E'…'` | MySQL |
//! |---|---|---|
//! | `\Z` | `Z` (5A) | **1A**, ctrl-Z |
//! | `\%` / `\_` | `%` / `_` | **5C25 / 5C5F** — the backslash is kept |
//! | `\xHH` / `\NNN` | decoded (41 / 41) | **not special**: 783431 / 313031 |
//!
//! All three produced silently wrong bytes rather than an error, and the
//! `\%` one is worse than it looks: the backslash is what makes LIKE treat
//! the wildcard literally, so dropping it changed which rows matched.

use spg_engine::Engine;
use spg_storage::Value;

fn hex_of(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err}")) {
        spg_engine::QueryResult::Rows { rows, .. } => match rows.first().and_then(|r| r.values.first()) {
            Some(Value::Text(t)) => {
                use std::fmt::Write as _;
                t.bytes().fold(String::new(), |mut acc, b| {
                    let _ = write!(acc, "{b:02X}");
                    acc
                })
            }
            other => panic!("`{sql}` did not return text: {other:?}"),
        },
        other => panic!("`{sql}` did not return rows: {other:?}"),
    }
}

fn mysql_session() -> Engine {
    let mut e = Engine::new();
    // What a mysql client's preamble does; it selects the dialect.
    e.execute("SET sql_mode='STRICT_TRANS_TABLES'").unwrap();
    e
}

#[test]
fn mysql_escapes_match_mariadb() {
    let mut e = mysql_session();
    for (lit, want) in [
        (r"\Z", "1A"),
        (r"\%", "5C25"),
        (r"\_", "5C5F"),
        (r"\x41", "783431"),
        (r"\101", "313031"),
        (r"\q", "71"),
    ] {
        assert_eq!(hex_of(&mut e, &format!("SELECT '{lit}'")), want, "for `{lit}`");
    }
}

/// The escapes both dialects share must not have moved.
#[test]
fn the_shared_escapes_are_unchanged() {
    let mut e = mysql_session();
    for (lit, want) in [
        (r"\n", "0A"),
        (r"\t", "09"),
        (r"\0", "00"),
        (r"\b", "08"),
        (r"\\", "5C"),
        (r"\'", "27"),
    ] {
        assert_eq!(hex_of(&mut e, &format!("SELECT '{lit}'")), want, "for `{lit}`");
    }
}

/// PG's `E'…'` keeps PG's table — this is the half that must NOT change.
#[test]
fn pg_escape_strings_keep_pgs_table() {
    let mut e = Engine::new();
    for (lit, want) in [
        (r"\Z", "5A"),
        (r"\%", "25"),
        (r"\x41", "41"),
        (r"\101", "41"),
        (r"\q", "71"),
        (r"\n", "0A"),
    ] {
        assert_eq!(hex_of(&mut e, &format!("SELECT E'{lit}'")), want, "for `{lit}`");
    }
}

/// The user-visible consequence of keeping the backslash: LIKE reads it as
/// "this wildcard is literal". Measured on MariaDB 11 — `'a%b' LIKE 'a\%b'`
/// is true and `'axb' LIKE 'a\%b'` is false.
#[test]
fn a_backslash_escaped_wildcard_still_escapes_in_like() {
    let mut e = mysql_session();
    let ask = |e: &mut Engine, sql: &str| -> Value<'static> {
        match e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err}")) {
            spg_engine::QueryResult::Rows { rows, .. } => rows
                .first()
                .and_then(|r| r.values.first())
                .cloned()
                .map(Value::into_owned)
                .unwrap_or(Value::Null),
            other => panic!("{other:?}"),
        }
    };
    assert_eq!(ask(&mut e, r"SELECT 'a%b' LIKE 'a\%b'"), Value::Bool(true));
    assert_eq!(ask(&mut e, r"SELECT 'axb' LIKE 'a\%b'"), Value::Bool(false));
}
