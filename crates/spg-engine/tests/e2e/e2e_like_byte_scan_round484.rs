//! read01 round 484 — the ASCII byte scan behind `LIKE '%…%'`.
//!
//! `str::find(&str)` runs the two-way algorithm and its SETUP is the cost:
//! the profile put `StrSearcher::new` at 14.6 % of self time on
//! `s LIKE '%_05%'`, rebuilt every row for a two-byte needle that is a
//! compile-time constant.
//!
//! An ASCII needle can be scanned as bytes instead, and the reason it is
//! sound is specific: a UTF-8 continuation byte is always >= 0x80, so an
//! ASCII byte match can never land inside a multi-byte character. These
//! pin that reasoning rather than the speedup — a byte scan that got it
//! wrong would match in the middle of a character, or panic slicing there.

use spg_engine::{Engine, QueryResult};

fn rows(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => rows
            .iter()
            .map(|r| {
                r.values
                    .iter()
                    .map(spg_engine::eval::value_to_text)
                    .collect::<Vec<_>>()
                    .join("|")
            })
            .collect::<Vec<_>>()
            .join(";"),
        other => panic!("{sql} -> {other:?}"),
    }
}

fn seeded() -> Engine {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT, s TEXT)").unwrap();
    e.execute(
        "INSERT INTO t VALUES \
         (1, 'user_0005'), \
         (2, 'user_0050'), \
         (3, 'plain'), \
         (4, '日本語テキスト'), \
         (5, 'caf\u{00e9} au lait'), \
         (6, 'aaaa'), \
         (7, ''), \
         (8, '\u{00e9}\u{00e9}\u{00e9}')",
    )
    .unwrap();
    e
}

#[test]
fn round484_the_shape_under_test_is_unchanged() {
    let mut e = seeded();
    // `%_05%` — one wildcard char, then the literal.
    assert_eq!(
        rows(&mut e, "SELECT id FROM t WHERE s LIKE '%_05%' ORDER BY id"),
        "1;2"
    );
    // Both seeded strings contain "005"; `%050%` is the one that
    // discriminates, and it is the no-wildcard variant of the same shape.
    assert_eq!(
        rows(&mut e, "SELECT id FROM t WHERE s LIKE '%005%' ORDER BY id"),
        "1;2"
    );
    assert_eq!(
        rows(&mut e, "SELECT id FROM t WHERE s LIKE '%050%' ORDER BY id"),
        "2"
    );
}

#[test]
fn round484_an_ascii_needle_never_matches_inside_a_character() {
    // The failure a byte scan invites. Every byte of these strings past
    // the first is a continuation byte >= 0x80, so no ASCII needle may
    // match — and slicing at a wrong offset would panic rather than
    // answer, which is the louder half of the same check.
    let mut e = seeded();
    for needle in ["a", "e", "z", "0", "ab"] {
        let got = rows(
            &mut e,
            &format!("SELECT id FROM t WHERE s LIKE '%{needle}%' AND id IN (4,8) ORDER BY id"),
        );
        assert_eq!(got, "", "'{needle}' must not match a multi-byte string");
    }
    // …while the ASCII part of a mixed string still matches normally.
    assert_eq!(
        rows(&mut e, "SELECT id FROM t WHERE s LIKE '%au lait%'"),
        "5"
    );
    assert_eq!(rows(&mut e, "SELECT id FROM t WHERE s LIKE '%caf%'"), "5");
}

#[test]
fn round484_a_non_ascii_needle_still_matches() {
    // Falls back to `str::find`, where the byte-boundary reasoning does
    // not hold.
    let mut e = seeded();
    assert_eq!(rows(&mut e, "SELECT id FROM t WHERE s LIKE '%本語%'"), "4");
    assert_eq!(
        rows(&mut e, "SELECT id FROM t WHERE s LIKE '%\u{00e9}\u{00e9}%'"),
        "8"
    );
    assert_eq!(
        rows(&mut e, "SELECT id FROM t WHERE s LIKE '%caf\u{00e9}%'"),
        "5"
    );
}

#[test]
fn round484_overlapping_candidates_and_the_wildcard_budget() {
    // `aaaa` gives the scan repeated first-byte hits; the `k` leading
    // wildcards then decide. `%_aa%` needs one char before the literal,
    // `%__aa%` needs two, `%_____aa%` more than the string has.
    let mut e = seeded();
    assert_eq!(rows(&mut e, "SELECT id FROM t WHERE s LIKE '%aa%'"), "6");
    assert_eq!(rows(&mut e, "SELECT id FROM t WHERE s LIKE '%_aa%'"), "6");
    assert_eq!(rows(&mut e, "SELECT id FROM t WHERE s LIKE '%__aa%'"), "6");
    assert_eq!(rows(&mut e, "SELECT id FROM t WHERE s LIKE '%___aa%'"), "");
    // Empty string matches only the unconstrained pattern.
    assert_eq!(
        rows(&mut e, "SELECT id FROM t WHERE s LIKE '%%' ORDER BY id"),
        "1;2;3;4;5;6;7;8"
    );
}

#[test]
fn round484_negation_and_null_are_unchanged() {
    let mut e = seeded();
    assert_eq!(
        rows(&mut e, "SELECT id FROM t WHERE s NOT LIKE '%0%' ORDER BY id"),
        "3;4;5;6;7;8"
    );
    e.execute("INSERT INTO t VALUES (9, NULL)").unwrap();
    // A NULL matches neither LIKE nor NOT LIKE.
    assert_eq!(rows(&mut e, "SELECT count(*) FROM t WHERE s LIKE '%a%'"), "3");
    assert_eq!(
        rows(&mut e, "SELECT count(*) FROM t WHERE s NOT LIKE '%a%'"),
        "5"
    );
}
