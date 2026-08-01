//! Round 669 — F33, and the ledger entry was nine rounds out of date.
//!
//! F33 was opened in round 627 as "not three items, the whole numeric
//! template subsystem", with 56 of 104 single-letter shapes and 13 of 24
//! keyword shapes recorded as divergent, and a warning to give it a round
//! of its own rather than touch the letter set casually.
//!
//! Re-measured against PG18 across the same 76 shapes: **six** differed.
//! Rounds 629 and 631 had already rebuilt most of it — their comments say
//! so — and nobody went back to the entry.
//!
//! Of those six, three were never defects. `L` is the locale currency
//! symbol. The oracle container runs `lc_monetary = en_US.utf8` and answers
//! `$`; SPG advertises `lc_monetary = C` and answers a space; and PG under
//! `SET lc_monetary='C'` answers a space too, byte for byte. The code
//! already carried a comment warning that measuring this oracle without
//! reading the GUC the feature depends on is how the wrong conclusion gets
//! drawn — which is exactly the mistake this round nearly repeated.
//!
//! The three that were real are fixed here: PG refuses a bare ordinal
//! suffix and refuses a picture with two `S`, and SPG answered both.

use spg_engine::{Engine, QueryResult};

fn one(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err}")) {
        QueryResult::Rows { rows, .. } => spg_engine::eval::value_to_text(&rows[0].values[0]),
        other => panic!("{sql}: {other:?}"),
    }
}

fn err(e: &mut Engine, sql: &str) -> String {
    format!("{}", e.execute(sql).expect_err(sql))
}

/// PG18: an ordinal suffix with no digit position before it is an error,
/// and the message is about the decimal point rather than the suffix.
#[test]
fn round669_a_bare_ordinal_suffix_is_refused() {
    let mut e = Engine::new();
    for f in ["TH", "th"] {
        let sql = format!("SELECT to_char(1234, '{f}')");
        assert!(
            err(&mut e, &sql).contains("\".\" is not a number"),
            "{}: {}",
            sql,
            err(&mut e, &sql)
        );
    }
    // With a digit position it is fine, and unchanged.
    assert_eq!(one(&mut e, "SELECT to_char(1234, '9999TH')"), " 1234TH");
    assert_eq!(one(&mut e, "SELECT to_char(1234, '9th')"), " #");
}

/// PG18: `cannot use "S" twice`, wherever the second one sits.
#[test]
fn round669_two_sign_positions_are_refused() {
    let mut e = Engine::new();
    for f in ["SS9999", "S9999S", "SS"] {
        let sql = format!("SELECT to_char(1234.5, '{f}')");
        assert!(
            err(&mut e, &sql).contains("cannot use \"S\" twice"),
            "{}: {}",
            sql,
            err(&mut e, &sql)
        );
    }
    // One `S` on either side still works.
    assert_eq!(one(&mut e, "SELECT to_char(1234.5, 'S9999')"), "+1235");
    assert_eq!(one(&mut e, "SELECT to_char(1234.5, '9999S')"), "1235+");
}

/// `L` is the LOCALE currency symbol, and SPG advertises `lc_monetary = C`.
///
/// Do not "fix" these to `$` by comparing against the differential oracle:
/// that container runs `en_US.utf8`. Under `SET lc_monetary='C'` PG18
/// answers exactly what is asserted here. Round 629 left a comment saying
/// so and round 669 still had to re-derive it.
#[test]
fn round669_the_currency_letter_follows_the_locale_spg_advertises() {
    let mut e = Engine::new();
    assert_eq!(one(&mut e, "SELECT current_setting('lc_monetary')"), "C");
    assert_eq!(one(&mut e, "SELECT to_char(1, 'L')"), " ");
    assert_eq!(one(&mut e, "SELECT to_char(1, 'L9')"), "  1");
    // A LITERAL `$` is not locale-dependent and still prints.
    assert_eq!(one(&mut e, "SELECT to_char(1, '$9')"), "$ 1");
}

/// The shapes the ledger said were broken and are not. Re-measured against
/// PG18; every one of these agrees.
#[test]
fn round669_the_letters_the_ledger_called_broken_all_agree() {
    let mut e = Engine::new();
    // "SPG treats E F H I M N P R T as templates and swallows them" —
    // measured, they echo, as on PG.
    for (f, want) in [
        ("HH", "HH"),
        ("MON", "MON"),
        ("DAY", " .AY"),
        ("RN", "        MCCXXXV"),
        ("EEEE", " 1e+03"),
        ("MI", " "),
        ("PL", "+"),
        ("PR", ""),
        ("SG", "+"),
        ("D", " ."),
        ("FM9", "#"),
        ("G9", "  #"),
        ("9G9", " #,#"),
        ("B9", " #"),
        ("C9", " #"),
    ] {
        let sql = format!("SELECT to_char(1234.5, '{f}')");
        assert_eq!(one(&mut e, &sql), want, "{sql}");
    }
}
