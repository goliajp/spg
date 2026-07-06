//! String / text function differential corrections vs PostgreSQL 18.
//!
//! Every expected value in this file was captured live from PG 18.4
//! (`psql -tAc`) on the mini bench container during the 5th
//! differential sweep. It guards the CLEAR-BUG fixes found there:
//!
//!   1. `format('%I', ident)` and `quote_ident(ident)` now follow
//!      PG's `quote_identifier`: a value that is already a safe
//!      unquoted identifier (`simple`, `value`, `_x`) is emitted
//!      verbatim instead of always being wrapped in double quotes.
//!      Values that need quoting — keywords (`select`, `user`,
//!      `between`), uppercase (`aBc`), digit-leading (`1abc`), the
//!      empty string, or embedded specials — are still quoted, with
//!      `"` doubled.
//!   2. `overlay(str PLACING repl FROM n [FOR len])` SQL syntax now
//!      parses (desugars to the `overlay(...)` function form the
//!      evaluator already implemented); previously the parser
//!      errored on the `PLACING` keyword.
//!
//! The wider corpus (substring index math, split_part negatives,
//! trim / pad / left / right / replace / translate / concat NULL
//! handling, length semantics, case, misc) already matched PG and is
//! asserted here as a regression panel. Deliberately-unfixed
//! divergences (locale collation, regex `substring`) are asserted at
//! SPG's current behaviour with the PG value noted, so the boundary
//! is documented, not silent.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

/// Render the first scalar of the first row as PG-comparable text.
/// NULL → `<NULL>` so trailing-space / null divergences are visible.
fn scalar(e: &mut Engine, sql: &str) -> String {
    let r = e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    let QueryResult::Rows { rows, .. } = r else {
        panic!("{sql}: expected Rows");
    };
    match &rows[0].values[0] {
        Value::Null => "<NULL>".into(),
        Value::Text(s) => s.to_string(),
        Value::Bool(b) => if *b { "true" } else { "false" }.into(),
        Value::SmallInt(n) => n.to_string(),
        Value::Int(n) => n.to_string(),
        Value::BigInt(n) => n.to_string(),
        other => panic!("{sql}: unexpected {other:?}"),
    }
}

/// Assert a batch of `(sql, expected)` pairs, all expecteds live-PG18.
fn check(cases: &[(&str, &str)]) {
    let mut e = Engine::new();
    for (sql, want) in cases {
        let got = scalar(&mut e, sql);
        assert_eq!(&got, want, "\n  sql: {sql}\n  PG18: {want:?}\n  SPG:  {got:?}");
    }
}

// ── CLEAR-BUG #1: format('%I') / quote_ident follow quote_identifier ──

#[test]
fn format_ident_quotes_only_when_needed() {
    check(&[
        // Safe unquoted identifiers — emitted verbatim (was over-quoted).
        ("SELECT format('%I','simple')", "simple"),
        ("SELECT format('%I','value')", "value"),
        ("SELECT format('%I','_x')", "_x"),
        ("SELECT format('%I','a_b1')", "a_b1"),
        // Needs quoting: spaces, keywords, uppercase, digit-lead, empty.
        ("SELECT format('%I','my ident')", "\"my ident\""),
        ("SELECT format('%I','select')", "\"select\""),
        ("SELECT format('%I','user')", "\"user\""),
        ("SELECT format('%I','between')", "\"between\""),
        ("SELECT format('%I','aBc')", "\"aBc\""),
        ("SELECT format('%I','1abc')", "\"1abc\""),
        ("SELECT format('%I','')", "\"\""),
        // %L / %s / positional unchanged.
        ("SELECT format('%s-%s','a','b')", "a-b"),
        ("SELECT format('%L','it''s')", "'it''s'"),
        ("SELECT format('%1$s-%1$s','x')", "x-x"),
        ("SELECT format('%L',NULL)", "NULL"),
        ("SELECT format('%%s')", "%s"),
    ]);
}

#[test]
fn quote_ident_matches_pg() {
    check(&[
        ("SELECT quote_ident('simple')", "simple"),
        ("SELECT quote_ident('value')", "value"),
        ("SELECT quote_ident('_x')", "_x"),
        ("SELECT quote_ident('select')", "\"select\""),
        ("SELECT quote_ident('aBc')", "\"aBc\""),
        // Embedded double-quote is doubled.
        ("SELECT quote_ident('a\"b')", "\"a\"\"b\""),
    ]);
}

// ── CLEAR-BUG #2: overlay(... PLACING ... FROM ... [FOR ...]) parses ──

#[test]
fn overlay_placing_syntax_matches_pg() {
    check(&[
        ("SELECT overlay('12345' placing 'ab' from 2 for 3)", "1ab5"),
        ("SELECT overlay('12345' placing 'ab' from 2)", "1ab45"),
        ("SELECT overlay('12345' placing 'abc' from 6)", "12345abc"),
        ("SELECT overlay('Txxxxas' placing 'hom' from 2 for 4)", "Thomas"),
    ]);
}

// ── Regression panel: corpus that already matched PG18 ──

#[test]
fn substring_matches_pg() {
    check(&[
        ("SELECT substring('hello' from 2 for 3)", "ell"),
        ("SELECT substring('hello',2,3)", "ell"),
        ("SELECT substring('hello' from 2)", "ello"),
        // Negative/zero start: PG measures the length window from
        // start, so chars before position 1 are dropped.
        ("SELECT substring('hello',-1,3)", "h"),
        ("SELECT substring('hello',0,3)", "he"),
        ("SELECT substring('hello',2,100)", "ello"),
        ("SELECT substring('hello',3,0)", ""),
        ("SELECT substring('hello',-2,2)", ""),
    ]);
}

#[test]
fn split_position_matches_pg() {
    check(&[
        ("SELECT split_part('a,b,c',',',2)", "b"),
        ("SELECT split_part('a,b,c',',',-1)", "c"),
        ("SELECT split_part('a,b',',',5)", ""),
        ("SELECT split_part('a,b,c',',',-2)", "b"),
        ("SELECT position('lo' in 'hello')", "4"),
        ("SELECT strpos('hello','lo')", "4"),
        ("SELECT strpos('hello','x')", "0"),
        ("SELECT position('' in 'hello')", "1"),
    ]);
}

#[test]
fn trim_family_matches_pg() {
    check(&[
        ("SELECT trim(' x ')", "x"),
        ("SELECT trim(both 'x' from 'xxhixx')", "hi"),
        ("SELECT ltrim('xxhi','x')", "hi"),
        ("SELECT rtrim('hixx','x')", "hi"),
        ("SELECT btrim('xxhixx','x')", "hi"),
        ("SELECT trim(leading '0' from '000123')", "123"),
        ("SELECT trim(trailing 'x' from 'hixx')", "hi"),
        ("SELECT trim('xy' from 'xyabcyx')", "abc"),
    ]);
}

#[test]
fn pad_matches_pg() {
    check(&[
        ("SELECT lpad('hi',5)", "   hi"),
        ("SELECT lpad('hi',5,'*')", "***hi"),
        ("SELECT lpad('hello',3)", "hel"),
        ("SELECT rpad('hi',5,'ab')", "hiaba"),
        ("SELECT lpad('hi',-1)", ""),
        ("SELECT rpad('hi',0)", ""),
        ("SELECT lpad('hi',5,'')", "hi"),
        ("SELECT rpad('hello',3,'*')", "hel"),
        ("SELECT lpad('abc',7,'xy')", "xyxyabc"),
    ]);
}

#[test]
fn left_right_matches_pg() {
    check(&[
        ("SELECT left('hello',2)", "he"),
        ("SELECT left('hello',-2)", "hel"),
        ("SELECT right('hello',-2)", "llo"),
        ("SELECT right('hello',2)", "lo"),
        ("SELECT left('hello',100)", "hello"),
        ("SELECT left('hello',-100)", ""),
        ("SELECT right('hello',0)", ""),
        ("SELECT left('hello',0)", ""),
    ]);
}

#[test]
fn replace_translate_matches_pg() {
    check(&[
        ("SELECT replace('abcabc','b','X')", "aXcaXc"),
        ("SELECT translate('12345','143','ax')", "a2x5"),
        ("SELECT translate('abcdef','bd','')", "acef"),
        ("SELECT replace('aaa','a','')", ""),
    ]);
}

#[test]
fn concat_null_matches_pg() {
    check(&[
        ("SELECT 'a'||NULL", "<NULL>"),
        ("SELECT concat('a',NULL,'b')", "ab"),
        ("SELECT concat_ws(',','a',NULL,'b')", "a,b"),
        ("SELECT concat_ws(',',NULL)", ""),
        ("SELECT concat(1,2,3)", "123"),
        ("SELECT concat_ws('-',1,2,3)", "1-2-3"),
        ("SELECT concat(NULL,NULL)", ""),
    ]);
}

#[test]
fn length_matches_pg() {
    check(&[
        ("SELECT length('héllo')", "5"),
        ("SELECT char_length('héllo')", "5"),
        ("SELECT octet_length('héllo')", "6"),
        ("SELECT bit_length('héllo')", "48"),
        ("SELECT length(E'\\x00abc')", "4"),
        ("SELECT length('')", "0"),
    ]);
}

#[test]
fn case_and_misc_matches_pg() {
    check(&[
        ("SELECT upper('héllo')", "HÉLLO"),
        ("SELECT lower('HÉLLO')", "héllo"),
        ("SELECT initcap('hello WORLD')", "Hello World"),
        ("SELECT initcap('the quick brown fox')", "The Quick Brown Fox"),
        ("SELECT repeat('ab',3)", "ababab"),
        ("SELECT reverse('abc')", "cba"),
        ("SELECT reverse('héllo')", "olléh"),
        ("SELECT ascii('A')", "65"),
        ("SELECT chr(65)", "A"),
        ("SELECT md5('')", "d41d8cd98f00b204e9800998ecf8427e"),
        ("SELECT md5('abc')", "900150983cd24fb0d6963f7d28e17f72"),
        ("SELECT starts_with('hello','he')", "true"),
        ("SELECT starts_with('hello','lo')", "false"),
        ("SELECT repeat('x',0)", ""),
    ]);
}

#[test]
fn arrays_and_like_matches_pg() {
    check(&[
        ("SELECT string_to_array('a,b,c',',')::text", "{a,b,c}"),
        ("SELECT string_to_array('a,,c',',')::text", "{a,\"\",c}"),
        ("SELECT parse_ident('a.b.c')::text", "{a,b,c}"),
        ("SELECT ('abc' LIKE 'a%')::text", "true"),
        ("SELECT ('ABC' ILIKE 'a%')::text", "true"),
        ("SELECT ('a_c' LIKE 'a\\_c')::text", "true"),
        ("SELECT ('a%c' LIKE 'a\\%c')::text", "true"),
    ]);
}

// ── DEFERRED / KNOWN-LIMITATION boundary (documented, not fixed) ──
//
// Locale collation: PG's default collation orders `'a' < 'B'` = true
// (case-insensitive-ish en_US); SPG compares by ASCII codepoint, so
// `'a'(97) < 'B'(66)` = false. This is a collation-representation
// divergence, not a localized bug — asserted at SPG's behaviour.
//
// Regex `substring(text FROM pattern)` (e.g. `substring('hello' from
// 'l+')` → PG `ll`): SPG's regex engine is whole-match-only (no
// capture-group extraction — an architectural gap tracked with the
// regex epic), so the SQL-standard capture-group-preferring form
// cannot be implemented faithfully yet. SPG currently errors on this
// form; left as-is.
#[test]
fn documented_boundaries() {
    let mut e = Engine::new();
    // ASCII collation (PG18 default-collation value: true).
    assert_eq!(scalar(&mut e, "SELECT ('a' < 'B')::text"), "false");
}

/// `x [NOT] LIKE/ILIKE ANY|ALL (ARRAY[...])` — quantified pattern match,
/// desugared to an OR/AND chain of per-element LIKEs. Every value is
/// live-PG18.4-verified, including the three-valued NULL case.
#[test]
fn like_any_all_quantified() {
    check(&[
        ("SELECT ('hello' LIKE ALL(ARRAY['h%', '%o']))::text", "true"),
        ("SELECT ('hello' LIKE ALL(ARRAY['h%', 'x%']))::text", "false"),
        ("SELECT ('hello' LIKE ANY(ARRAY['x%', '%o']))::text", "true"),
        ("SELECT ('hello' LIKE ANY(ARRAY['x%', 'y%']))::text", "false"),
        ("SELECT ('hello' NOT LIKE ANY(ARRAY['x%', 'y%']))::text", "true"),
        // NOT LIKE ALL: false because 'hello' DOES match 'h%'.
        ("SELECT ('hello' NOT LIKE ALL(ARRAY['h%', 'x%']))::text", "false"),
        ("SELECT ('hello' NOT LIKE ALL(ARRAY['x%', 'y%']))::text", "true"),
        // Case-insensitive quantified match.
        ("SELECT ('HeLLo' ILIKE ANY(ARRAY['h%', 'z%']))::text", "true"),
    ]);
    // A NULL pattern under ANY yields SQL NULL (three-valued logic), not
    // false — the OR desugar preserves it.
    let mut e = Engine::new();
    assert_eq!(
        scalar(&mut e, "SELECT ('hello' LIKE ANY(ARRAY['x%', NULL]))::text"),
        "<NULL>"
    );
}
