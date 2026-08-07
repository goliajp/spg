//! read01 round 488 — `<column> [NOT] [I]LIKE '<literal>'` matched in place.
//!
//! `like_filter` was the read panel's worst shape. It compiles to two
//! steps, `Column` then `Like`/`LikeSubstring`, which neither round 482's
//! `<column> <cmp> <literal>` nor round 486's `<column> IN (…)` fast path
//! covers, so it ran the general VM: the cell pushed as a `Value` and
//! popped again for a matcher that only ever wanted a `&str`.
//!
//! The fast path calls the SAME `like_match_str` / `like_substring_match`
//! the steps call — the arms are untouched, because round 486 measured
//! that editing that loop costs unrelated shapes. What is restated is the
//! NULL and negation wrapper, so every case below runs down BOTH paths and
//! asserts they agree. `probe_like_shape` establishes with a counter which
//! spelling is which.
//!
//! Expectations are PG18's, read off `psql -tA`. Four of them were wrong
//! when drafted: 'USER_0050' also contains '05', and the multi-byte row
//! is five CHARACTERS long, so it matches `_____`. Asked, not reasoned.

use spg_engine::{Engine, QueryResult};

fn ids(e: &mut Engine, sql: &str) -> String {
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
    e.execute("CREATE TABLE r (id INT, s TEXT, c CHAR(6))")
        .unwrap();
    e.execute(
        "INSERT INTO r VALUES \
         (1, 'user_0005', 'ab'), \
         (2, 'USER_0050', 'ab   '), \
         (3, 'plain', 'abc'), \
         (4, NULL, NULL), \
         (5, '\u{65e5}\u{672c}\u{8a9e}05', 'b'), \
         (6, '', '')",
    )
    .unwrap();
    e
}

/// Run the same predicate through the fast path and through the general
/// VM, assert both give `expected`. `pred` must be a bare
/// `<column> [NOT] [I]LIKE '<literal>'`.
fn both_paths(e: &mut Engine, pred: &str, expected: &str) {
    let fast = format!("SELECT id FROM r WHERE {pred} ORDER BY id");
    let general = format!("SELECT id FROM r WHERE ({pred}) = true ORDER BY id");
    assert_eq!(ids(e, &fast), expected, "fast path: {pred}");
    assert_eq!(ids(e, &general), expected, "general path: {pred}");
}

#[test]
fn round488_substring_shape() {
    let mut e = seeded();
    both_paths(&mut e, "s LIKE '%05%'", "1;2;5");
    both_paths(&mut e, "s LIKE '%_05%'", "1;2;5");
    both_paths(&mut e, "s NOT LIKE '%05%'", "3;6");
}

#[test]
fn round488_general_matcher_shape() {
    let mut e = seeded();
    both_paths(&mut e, "s LIKE 'user%'", "1");
    both_paths(&mut e, "s LIKE '%n'", "3");
    both_paths(&mut e, "s LIKE '_____'", "3;5");
    both_paths(&mut e, "s LIKE ''", "6");
}

#[test]
fn round488_case_insensitive() {
    let mut e = seeded();
    both_paths(&mut e, "s ILIKE 'user%'", "1;2");
    both_paths(&mut e, "s ILIKE '%USER%'", "1;2");
    both_paths(&mut e, "s NOT ILIKE 'user%'", "3;5;6");
}

#[test]
fn round488_null_matches_neither_side() {
    // Row 4's `s` is NULL: neither LIKE nor NOT LIKE selects it, and the
    // fast path must reach that through the same three-valued logic the
    // VM does rather than answering false directly.
    let mut e = seeded();
    both_paths(&mut e, "s LIKE '%'", "1;2;3;5;6");
    both_paths(&mut e, "s NOT LIKE '%'", "");
    both_paths(&mut e, "s NOT LIKE '%05%'", "3;6");
}

/// An all-`%` pattern is NULL for a NULL operand, not FALSE.
///
/// v7.36 collapsed `x [NOT] LIKE '%'` into an `IS [NOT] NULL` test, which
/// is two-valued. `WHERE s NOT LIKE '%'` therefore SELECTED the NULL row
/// — PG18 selects nothing — and the projected value read `false` where PG
/// reads NULL. Found by the round-488 pins; the shortcut predates them.
#[test]
fn round488_all_percent_pattern_is_three_valued() {
    let mut e = seeded();
    // PG18: NULL::text LIKE '%' -> NULL, NOT LIKE '%' -> NULL, and both
    // `IS NULL`.
    assert_eq!(
        ids(&mut e, "SELECT (s LIKE '%')::text AS a FROM r WHERE id = 4"),
        "NULL"
    );
    assert_eq!(
        ids(
            &mut e,
            "SELECT (s NOT LIKE '%')::text AS a FROM r WHERE id = 4"
        ),
        "NULL"
    );
    assert_eq!(
        ids(
            &mut e,
            "SELECT ((s LIKE '%') IS NULL)::text AS a FROM r WHERE id = 4"
        ),
        "true"
    );
    // Every all-% spelling, and ILIKE too.
    for pat in ["%", "%%", "%%%"] {
        both_paths(&mut e, &format!("s LIKE '{pat}'"), "1;2;3;5;6");
        both_paths(&mut e, &format!("s NOT LIKE '{pat}'"), "");
        both_paths(&mut e, &format!("s ILIKE '{pat}'"), "1;2;3;5;6");
    }
    // Non-NULL operands keep the two answers the collapse always gave.
    assert_eq!(
        ids(&mut e, "SELECT (s LIKE '%')::text AS a FROM r WHERE id = 6"),
        "true"
    );
    assert_eq!(
        ids(
            &mut e,
            "SELECT (s NOT LIKE '%')::text AS a FROM r WHERE id = 6"
        ),
        "false"
    );
}

#[test]
fn round488_bpchar_matches_its_padded_form() {
    // PG's bpchar pattern operators match the PADDED stored form, so
    // 'ab'::char(6) is 'ab    ' and does not match 'ab'.
    let mut e = seeded();
    both_paths(&mut e, "c LIKE 'ab'", "");
    both_paths(&mut e, "c LIKE 'ab%'", "1;2;3");
    both_paths(&mut e, "c LIKE 'ab____'", "1;2;3");
}

#[test]
fn round488_multibyte_haystack() {
    // The round-484 byte scan must not match inside a multi-byte
    // character; '05' here follows three multi-byte chars.
    let mut e = seeded();
    both_paths(&mut e, "s LIKE '%\u{8a9e}05'", "5");
    both_paths(&mut e, "s LIKE '\u{65e5}%'", "5");
}

#[test]
fn round488_non_text_operand_still_errors() {
    // The fast path declines a non-text cell and lets the VM raise the
    // type error in its own wording.
    let mut e = seeded();
    e.execute("CREATE TABLE n (id INT, v INT)").unwrap();
    e.execute("INSERT INTO n VALUES (1, 105)").unwrap();
    let r = e.execute("SELECT id FROM n WHERE v LIKE '%05%'");
    assert!(r.is_err(), "int LIKE -> {r:?}");
}
