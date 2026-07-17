//! v7.38 (read01, T11) — CHAR(n) / bpchar semantics: blank-padded storage, but
//! length / comparison / DISTINCT / ::text / concat all ignore the trailing
//! blanks; an over-long cast truncates. Oracle: live PG 18.4.

use spg_engine::{Engine, QueryResult};

fn one(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => match &rows[0].values[0] {
            spg_storage::Value::Text(s) | spg_storage::Value::BpChar(s) => s.to_string(),
            spg_storage::Value::Bool(b) => b.to_string(),
            spg_storage::Value::Int(n) => n.to_string(),
            spg_storage::Value::BigInt(n) => n.to_string(),
            v => format!("{v:?}"),
        },
        _ => panic!("rows"),
    }
}

#[test]
fn bpchar_semantics() {
    let mut e = Engine::new();
    // Comparison is trailing-space-insensitive.
    assert_eq!(one(&mut e, "SELECT 'ab'::char(4) = 'ab'"), "true");
    assert_eq!(one(&mut e, "SELECT 'ab'::char(4) = 'ab  '"), "true");
    assert_eq!(one(&mut e, "SELECT 'ab'::char(4) = 'ab'::char(6)"), "true");
    assert_eq!(one(&mut e, "SELECT 'ab'::char(4) < 'ac'"), "true");
    // length / char_length ignore padding.
    assert_eq!(one(&mut e, "SELECT length('ab'::char(4))"), "2");
    assert_eq!(one(&mut e, "SELECT char_length('ab'::char(4))"), "2");
    // ::text strips; concat strips.
    assert_eq!(one(&mut e, "SELECT ('ab'::char(4))::text"), "ab");
    assert_eq!(one(&mut e, "SELECT length(('ab'::char(4))::text)"), "2");
    assert_eq!(one(&mut e, "SELECT 'ab'::char(4) || 'x'"), "abx");
    assert_eq!(
        one(&mut e, "SELECT '[' || ('ab'::char(4))::text || ']'"),
        "[ab]"
    );
    // Over-long cast truncates.
    assert_eq!(one(&mut e, "SELECT 'abc'::char(2)"), "ab");
    assert_eq!(one(&mut e, "SELECT length('abc'::char(2))"), "2");
    // ORDER BY / equality ignore trailing blanks.
    assert_eq!(one(&mut e, "SELECT 'ab'::char(4) = 'ab'::char(2)"), "true");
    // DISTINCT / GROUP BY dedup blank-insensitively, even across declared widths
    // and against plain text (R3).
    assert_eq!(
        one(
            &mut e,
            "SELECT count(DISTINCT x) FROM (VALUES('ab'::char(4)),('ab'::char(6)),('ab'::char(2))) v(x)"
        ),
        "1"
    );
    assert_eq!(
        one(
            &mut e,
            "SELECT count(DISTINCT x) FROM (VALUES('ab'::char(4)),('ab'::text)) v(x)"
        ),
        "1"
    );
}

// ── v7.39 bpchar epic — the differential-locked gap set (oracle: PG18.4) ──

#[test]
fn bpchar_octet_and_bit_length_count_padded() {
    let mut e = Engine::new();
    assert_eq!(one(&mut e, "SELECT octet_length('ab'::char(5))"), "5");
    assert_eq!(one(&mut e, "SELECT bit_length('ab'::char(5))"), "40");
}

#[test]
fn bpchar_like_matches_padded_form() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE c (a CHAR(5))").unwrap();
    e.execute("INSERT INTO c VALUES ('ab')").unwrap();
    assert_eq!(one(&mut e, "SELECT a LIKE 'ab' FROM c"), "false");
    assert_eq!(one(&mut e, "SELECT a LIKE 'ab%' FROM c"), "true");
    assert_eq!(one(&mut e, "SELECT a LIKE 'ab   ' FROM c"), "true");
    assert_eq!(one(&mut e, "SELECT a LIKE '%b' FROM c"), "false");
}

#[test]
fn bpchar_text_functions_see_stripped_form() {
    let mut e = Engine::new();
    assert_eq!(one(&mut e, "SELECT upper('ab'::char(5))"), "AB");
    assert_eq!(one(&mut e, "SELECT length(upper('ab'::char(5)))"), "2");
    assert_eq!(
        one(&mut e, "SELECT substring('ab'::char(5) from 1 for 4)"),
        "ab"
    );
    assert_eq!(one(&mut e, "SELECT position(' ' in 'ab'::char(5))"), "0");
    // concat keeps the padded form (PG output function).
    assert_eq!(one(&mut e, "SELECT concat('ab'::char(5), 'x')"), "ab   x");
    // pg_typeof must see the bpchar value, not its text image.
    assert_eq!(one(&mut e, "SELECT pg_typeof('ab'::char(5))"), "character");
}

#[test]
fn bpchar_cast_to_varchar_strips() {
    let mut e = Engine::new();
    assert_eq!(one(&mut e, "SELECT ('ab'::char(5))::varchar"), "ab");
    assert_eq!(one(&mut e, "SELECT length(('ab'::char(5))::varchar)"), "2");
}

#[test]
fn bpchar_insert_overflow_trims_blanks_else_22001() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE c (a CHAR(5))").unwrap();
    // Trailing-blank overflow is trimmed to fit (PG semantics).
    e.execute("INSERT INTO c VALUES ('abcd  ')")
        .expect("blank-only overflow must fit");
    // Real overflow is an error with PG's phrasing (22001 on the wire).
    let err = e
        .execute("INSERT INTO c VALUES ('abcdef')")
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("value too long for type character(5)"),
        "got: {err}"
    );
}

#[test]
fn bpchar_order_by_sorts_stripped() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE c (a CHAR(5), tag INT)").unwrap();
    e.execute("INSERT INTO c VALUES ('b', 1), ('a', 2), ('ab', 3)")
        .unwrap();
    let QueryResult::Rows { rows, .. } = e.execute("SELECT tag FROM c ORDER BY a").unwrap() else {
        panic!("rows")
    };
    let got: Vec<_> = rows.iter().map(|r| r.values[0].clone()).collect();
    assert_eq!(
        got,
        vec![
            spg_storage::Value::Int(2),
            spg_storage::Value::Int(3),
            spg_storage::Value::Int(1)
        ],
        "'a' < 'ab' < 'b' under stripped bpcharcmp"
    );
}

#[test]
fn bare_char_cast_is_char_1() {
    let mut e = Engine::new();
    assert_eq!(one(&mut e, "SELECT 'xyz'::char"), "x");
    assert_eq!(one(&mut e, "SELECT length('xyz'::char)"), "1");
}

#[test]
fn bpchar_format_keeps_pad_quote_literal_strips() {
    let mut e = Engine::new();
    // format renders via the output function (padded) —
    // differential-verified vs PG18.
    assert_eq!(
        one(&mut e, "SELECT format('<%s>', 'ab'::char(5))"),
        "<ab   >"
    );
    assert_eq!(one(&mut e, "SELECT quote_literal('ab'::char(5))"), "'ab'");
    assert_eq!(one(&mut e, "SELECT replace('ab'::char(5), 'b', 'X')"), "aX");
}

#[test]
fn varchar_overflow_cuts_blanks_at_limit_else_22001() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE vc (v VARCHAR(5))").unwrap();
    // PG keeps 'abcd ' (cut AT the limit, not a full strip).
    e.execute("INSERT INTO vc VALUES ('abcd  ')")
        .expect("blank-only overflow must fit");
    assert_eq!(one(&mut e, "SELECT '<' || v || '>' FROM vc"), "<abcd >");
    assert_eq!(one(&mut e, "SELECT length(v) FROM vc"), "5");
    let err = e
        .execute("INSERT INTO vc VALUES ('abcdef')")
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("value too long for type character varying(5)"),
        "got: {err}"
    );
}

#[test]
fn bare_char_column_is_char_1() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE bc (a CHAR, b CHARACTER)").unwrap();
    e.execute("INSERT INTO bc VALUES ('x', 'y')").unwrap();
    assert_eq!(one(&mut e, "SELECT a FROM bc"), "x");
    let err = e
        .execute("INSERT INTO bc VALUES ('xx', 'y')")
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("value too long for type character(1)"),
        "got: {err}"
    );
}
