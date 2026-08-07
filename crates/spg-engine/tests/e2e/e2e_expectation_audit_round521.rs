//! v7.39 (round 521) — expectations the suite had that PG18 does not share.
//!
//! Five rounds found a test asserting SPG's output rather than PG's answer
//! (r392, r504, r517, r518, r520), every one by accident: the test only
//! surfaced because a fix made it fail. A test that pins our own behaviour
//! is worse than no test — it makes the wrong answer load-bearing.
//!
//! `scripts/test-expectation-audit.py` asks directly. It runs every `SELECT`
//! literal in the e2e suite against BOTH servers and reports the
//! disagreements: 8044 literals, 1232 comparable, 36 disagreeing. Most are
//! environmental — the clock, the role name, catalog contents — and the
//! rest are these.
//!
//! Every expectation below is a PG18 reading.

use spg_engine::{Engine, QueryResult};

fn engine() -> Engine {
    Engine::new()
}

fn text(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err}")) {
        QueryResult::Rows { rows, .. } => rows
            .first()
            .map(|r| {
                r.values
                    .iter()
                    .map(spg_engine::eval::value_to_text)
                    .collect::<Vec<_>>()
                    .join("|")
            })
            .unwrap_or_default(),
        other => panic!("{sql}: {other:?}"),
    }
}

/// Case FOLDING is not lowercasing, and the difference is visible in Greek.
/// Folding is position-blind on purpose — its whole job is to make two
/// spellings of the same word compare equal — so a final sigma does not
/// appear where lowercasing would put one.
#[test]
fn round521_casefold_is_not_lowercase() {
    let mut e = engine();
    assert_eq!(text(&mut e, "SELECT casefold('ΣΟΦΟΣ')"), "σοφοσ");
    // An already-lowered final sigma is left alone.
    assert_eq!(text(&mut e, "SELECT casefold('σοφος')"), "σοφος");
    // A dotted capital I folds to plain `i`, not `i` plus a combining dot.
    assert_eq!(text(&mut e, "SELECT casefold('İSTANBUL')"), "istanbul");
    // Lowercasing still does what lowercasing does.
    assert_eq!(text(&mut e, "SELECT lower('ΣΟΦΟΣ')"), "σοφοσ");
}

/// A bpchar's padding does not count toward its bit length — and only that
/// one measure ignores it.
#[test]
fn round521_bit_length_ignores_bpchar_padding() {
    let mut e = engine();
    assert_eq!(
        text(
            &mut e,
            "SELECT bit_length('ab'::char(5)), length('ab'::char(5)), \
             octet_length('ab'::char(5)), bit_length('ab'::text)"
        ),
        "16|2|5|16"
    );
}

/// PG joins the hundreds to what follows with "and".
#[test]
fn round521_cash_words_says_and() {
    let mut e = engine();
    assert_eq!(
        text(&mut e, "SELECT cash_words('114.06')"),
        "One hundred and fourteen dollars and six cents"
    );
    // Only when something follows the hundreds.
    assert_eq!(
        text(&mut e, "SELECT cash_words('100.00')"),
        "One hundred dollars and zero cents"
    );
    assert_eq!(
        text(&mut e, "SELECT cash_words('1.00')"),
        "One dollar and zero cents"
    );
}

/// PG's pretty array opens on the same line as the first element and closes
/// on the last.
#[test]
fn round521_array_to_json_pretty_shape() {
    let mut e = engine();
    assert_eq!(
        text(&mut e, "SELECT array_to_json(ARRAY[1,2,3], true)"),
        "[1,\n 2,\n 3]"
    );
    assert_eq!(
        text(&mut e, "SELECT array_to_json(ARRAY[1,2,3], false)"),
        "[1,2,3]"
    );
}

/// `strip` takes a TSVECTOR, so an unknown literal becomes one — it used to
/// walk the string and hand back TEXT, which reads the same and is not a
/// tsvector, so nothing downstream could treat it as one.
#[test]
fn round521_strip_answers_a_tsvector() {
    let mut e = engine();
    assert_eq!(text(&mut e, "SELECT strip('cat dog')::text"), "'cat' 'dog'");
    assert_eq!(
        text(&mut e, "SELECT pg_typeof(strip('cat dog'))"),
        "tsvector"
    );
    // And the positions really are gone.
    assert_eq!(
        text(&mut e, "SELECT strip('cat:3 dog:7'::tsvector)::text"),
        "'cat' 'dog'"
    );
}
