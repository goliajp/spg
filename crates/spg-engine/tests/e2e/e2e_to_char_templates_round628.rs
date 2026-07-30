//! v7.39 (round 628, F33) — whether a letter is part of a number picture is
//! a question about the POSITION, not the character.
//!
//! `to_char(1,'MON')` answered a single space where PG answers `MON`, and
//! `to_char(1,'HH')` likewise. The predicate deciding it asked per
//! character, with `E R N T H P M I F` counted as template letters because
//! they appear inside `EEEE` `RN` `TH` `PL`/`PR` `MI` `FM`. PG matches
//! keywords left to right, longest first: a lone `M` is a literal and `MI`
//! is the minus column, a lone `P` is a literal and `PL` is the plus
//! column. A per-character question cannot tell those apart, so every one
//! of those letters was swallowed on its own.
//!
//! Measured over the alphabet in two placements (104 shapes) and over 24
//! multi-character keywords:
//!
//!     single letters   56 differing -> 20
//!     keywords         13 differing -> 10
//!
//! The scanner marks keyword bytes in one forward pass. Writing it as
//! "does a keyword START here" while walking BACKWARDS for the suffix was
//! the first cut, and it broke `MI` `RN` and `EEEE` — the `I` of `MI` looks
//! like a literal from that end — so they echoed instead of formatting.
//!
//! One thing this round changed and changed back: `L`, the locale currency
//! symbol, renders as a space here, and the bench oracle answers `$`. That
//! looked like a divergence until the GUC it depends on was read — the
//! oracle runs `lc_monetary = en_US.utf8`, the same PG with
//! `SET lc_monetary = 'C'` answers a space, and SPG reports `C`. The space
//! agrees with the locale SPG advertises; measuring the oracle without
//! reading the setting the feature depends on is what produced the wrong
//! conclusion. What DID survive is the currency column on an overflowed
//! body, which an early return used to drop.
//!
//! Recorded, not closed, and measured: `B` and `C` are picture elements PG
//! consumes and this treats as literals (`to_char(1,'abc')` is `abc` where
//! PG says `a`); `G` is positional in PG (`to_char(12,'9G9')` is ` 1,2`)
//! and enables thousands grouping here; `V` alone leaves a space where PG
//! leaves nothing; and a picture that is ONLY a sign column keeps a blank
//! slot PG omits.

use spg_engine::{Engine, QueryResult};

fn vals(e: &mut Engine, sql: &str) -> Vec<String> {
    match e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err}")) {
        QueryResult::Rows { rows, .. } => rows
            .iter()
            .map(|r| {
                r.values
                    .iter()
                    .map(spg_engine::eval::value_to_text)
                    .collect::<Vec<_>>()
                    .join("|")
            })
            .collect(),
        other => panic!("{sql}: {other:?}"),
    }
}

/// Letters that only matter inside a keyword are literals on their own.
#[test]
fn round628_lone_letters_are_literals() {
    let mut e = Engine::new();
    assert_eq!(
        vals(&mut e, "SELECT to_char(1,'MON'), to_char(1,'HH'), to_char(1,'E')"),
        vec!["MON|HH|E"],
        "PG echoes each of these; they were swallowed to a space"
    );
    assert_eq!(
        vals(&mut e, "SELECT to_char(1,'P'), to_char(1,'M'), to_char(1,'T'), to_char(1,'R')"),
        vec!["P|M|T|R"]
    );
    assert_eq!(
        vals(&mut e, "SELECT to_char(1,'xEy'), to_char(1,'xMy'), to_char(1,'xHy')"),
        vec!["xEy|xMy|xHy"]
    );
}

/// …and the keywords they appear in still format.
#[test]
fn round628_keywords_still_format() {
    let mut e = Engine::new();
    assert_eq!(
        vals(&mut e, "SELECT to_char(1234.5,'RN')"),
        vec!["        MCCXXXV"],
        "RN would echo as text if the suffix scan mistook its N for a literal"
    );
    assert_eq!(vals(&mut e, "SELECT to_char(1234.5,'EEEE')"), vec![" 1e+03"]);
    assert_eq!(vals(&mut e, "SELECT to_char(-1234.5,'9999.9PR')"), vec!["<1234.5>"]);
    assert_eq!(vals(&mut e, "SELECT to_char(-1234.5,'9999.9MI')"), vec!["1234.5-"]);
    assert_eq!(vals(&mut e, "SELECT to_char(1234.5,'FM9999.9')"), vec!["1234.5"]);
    assert_eq!(vals(&mut e, "SELECT to_char(1234,'9,999'), to_char(1234,'9G999')"), vec![" 1,234| 1,234"]);
}

/// The all-literal pattern round 626 stopped crashing on, still literal.
#[test]
fn round628_all_literal_patterns_survive() {
    let mut e = Engine::new();
    assert_eq!(
        vals(&mut e, "SELECT to_char(1,'YYYY'), to_char(1.5,'xyz'), to_char(1,'Q')"),
        vec!["YYYY|xyz|Q"]
    );
    assert_eq!(vals(&mut e, "SELECT to_char(12,'DAY')"), vec![" .AY"]);
}
