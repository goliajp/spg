//! Type-cast + implicit-coercion PG18 differential corpus
//! (11th differential sweep).
//!
//! Ground truth captured from live PostgreSQL 18.4 on 2026-07-04 (mini
//! docker `spg-bench-postgres`). Each `ck` asserts SPG's rendered
//! output (`(expr)::text`, so both sides share the type-output
//! function) against the exact string PG produced. `"ERR"` means PG
//! raised on that input and SPG must reject it too.
//!
//! BUGS FIXED in the accompanying commit and locked here:
//!   * `'2024-02-30'::date` / `'2024-04-31'::date` / `'2023-02-29'::date`
//!     — the fixed-position date parser only bounded the day to
//!     `1..=31`, so an over-length day silently rolled forward
//!     (Feb 30 -> Mar 1) instead of erroring. Now validated against the
//!     leap-aware month length.
//!   * `'2024-01-15 10:30'::timestamp` / `'10:30'::time` — the time
//!     parser required a full `HH:MM:SS`; PG accepts the
//!     seconds-optional `HH:MM` form (seconds default 0). Hour-only
//!     (`'... 10'`) still errors, matching PG.
//!   * `'  256  '::int2` / `'  3.14  '::float8` — int2 / float4 / float8
//!     casts route through the generic coerce path (unlike `::int` /
//!     `::float`, which trim in the CAST helper) and did not trim
//!     surrounding whitespace; PG does.
//!   * `'inf'::float8` / `'-inf'::float8` rendered `inf` / `-inf`; PG's
//!     `float8out` spells them `Infinity` / `-Infinity` (`NaN` already
//!     matched). Fixed in `value_to_text`.
//!
//! DEFERRED divergences (reported, NOT fixed — each needs a broad
//! refactor, so out of scope for a localized cast fix):
//!   * NUMERIC-LITERAL TYPING (root cause of several rows): SPG types a
//!     bare decimal literal (`2.5`, `12.50`) as Float, where PG types it
//!     as `numeric`. Consequences seen here:
//!       - `12.50::numeric::text` -> PG `12.50` (scale preserved),
//!         SPG `12.5` (trailing zero lost through the Float round-trip).
//!       - float8/float4 -> int/bigint uses round-half-AWAY where PG
//!         uses round-half-to-EVEN (`2.5::float8::int` PG=2 SPG=3,
//!         `0.5`->0 vs 1, `4.5`->4 vs 5, `-2.5`->-2 vs -3). A localized
//!         change to the Float cast arm would regress the dominant
//!         `2.5::int`=3 case, because SPG can't tell an explicit
//!         `::float8` value from a bare literal. (SEMANTIC.)
//!   * implicit unknown-string coercion in comparison / arithmetic:
//!     `1 = '1'`, `1 < '2'`, `1 + '2'` — PG coerces the unknown-typed
//!     literal to the other operand's type (true/true/3); SPG errors.
//!     This is the flagged SELECT-path type-widening gap (SEMANTIC).
//!   * `CASE WHEN true THEN 'a' ELSE 1 END` — PG unifies the branch
//!     types (and here errors: 'a' is not a valid integer); SPG returns
//!     the first branch as-is (`a`). Needs CASE/UNION type unification
//!     (SEMANTIC).
//!   * `'{"a":1}'::jsonb` — PG canonicalizes jsonb (space after the
//!     colon -> `{"a": 1}`); SPG round-trips the raw text. Needs a jsonb
//!     normalizer (KNOWN-LIMITATION).
//!   * `'0x10'::int` -> 16, `'1_000'::int` -> 1000 — PG16+ accepts
//!     hex/oct/bin prefixes and underscore digit grouping in integer
//!     text input; SPG uses plain decimal parse (KNOWN-LIMITATION).
//!   * `'2024-1-5'::date` -> 2024-01-05 — PG accepts non-zero-padded
//!     month/day; SPG requires the fixed 10-char `YYYY-MM-DD` shape
//!     (KNOWN-LIMITATION).
//!   * `true::bigint` — PG has no bool->bigint cast (errors); SPG
//!     accepts it as 1. SPG is laxer than PG here (reported).
//!   * `'2024-01-15 10:30'::timestamptz` -> PG appends the session-tz
//!     offset (`10:30:00+00`); SPG stores TIMESTAMP/TIMESTAMPTZ in one
//!     tz-naive representation and renders without the offset. Value is
//!     correct; the `+00` suffix needs tz-aware storage
//!     (KNOWN-LIMITATION). (This row only became reachable after the
//!     `HH:MM` parser fix above — it previously errored.)

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn cast(e: &mut Engine, expr: &str) -> String {
    let sql = format!("SELECT ({expr})::text AS r");
    match e.execute(&sql) {
        Ok(QueryResult::Rows { rows, .. }) => {
            if rows.is_empty() {
                return "<NOROWS>".into();
            }
            match &rows[0].values[0] {
                Value::Null => "<NULL>".into(),
                Value::Text(s) => s.to_string(),
                other => format!("<UNEXP:{other:?}>"),
            }
        }
        Ok(other) => format!("<NONROWS:{other:?}>"),
        Err(_) => "ERR".into(),
    }
}

fn ck(e: &mut Engine, expr: &str, want: &str) {
    let got = cast(e, expr);
    assert_eq!(
        got, want,
        "\n  expr: {expr}\n  want(PG18): {want}\n  got(SPG):   {got}"
    );
}

#[test]
fn cast_pg18_differential_corpus() {
    let mut e = Engine::new();
    ck(&mut e, r#"'123'::int"#, r#"123"#);
    ck(&mut e, r#"'123'::bigint"#, r#"123"#);
    ck(&mut e, r#"'12.5'::numeric"#, r#"12.5"#);
    ck(&mut e, r#"'12.5'::int"#, r#"ERR"#);
    ck(&mut e, r#"'12.9'::int"#, r#"ERR"#);
    ck(&mut e, r#"'  42  '::int"#, r#"42"#);
    ck(&mut e, r#"'abc'::int"#, r#"ERR"#);
    ck(&mut e, r#"123::text"#, r#"123"#);
    ck(&mut e, r#"12.5::text"#, r#"12.5"#);
    ck(&mut e, r#"123::bool"#, r#"true"#);
    ck(&mut e, r#"1::bool"#, r#"true"#);
    ck(&mut e, r#"0::bool"#, r#"false"#);
    ck(&mut e, r#"-1::bool"#, r#"ERR"#);
    ck(&mut e, r#"'t'::bool"#, r#"true"#);
    ck(&mut e, r#"'true'::bool"#, r#"true"#);
    ck(&mut e, r#"'yes'::bool"#, r#"true"#);
    ck(&mut e, r#"'y'::bool"#, r#"true"#);
    ck(&mut e, r#"'1'::bool"#, r#"true"#);
    ck(&mut e, r#"'on'::bool"#, r#"true"#);
    ck(&mut e, r#"'f'::bool"#, r#"false"#);
    ck(&mut e, r#"'no'::bool"#, r#"false"#);
    ck(&mut e, r#"'n'::bool"#, r#"false"#);
    ck(&mut e, r#"'off'::bool"#, r#"false"#);
    ck(&mut e, r#"'FALSE'::bool"#, r#"false"#);
    ck(&mut e, r#"'  t  '::bool"#, r#"true"#);
    ck(&mut e, r#"'2'::bool"#, r#"ERR"#);
    ck(&mut e, r#"true::int"#, r#"1"#);
    ck(&mut e, r#"false::int"#, r#"0"#);
    ck(&mut e, r#"true::text"#, r#"true"#);
    ck(&mut e, r#"'2024-01-15'::date"#, r#"2024-01-15"#);
    ck(&mut e, r#"'2024-01-15'::date::text"#, r#"2024-01-15"#);
    ck(&mut e, r#"'2024-01-15 10:30'::timestamp"#, r#"2024-01-15 10:30:00"#);
    ck(&mut e, r#"'10:30:00'::time"#, r#"10:30:00"#);
    ck(&mut e, r#"'invalid'::date"#, r#"ERR"#);
    ck(&mut e, r#"'2024-13-01'::date"#, r#"ERR"#);
    ck(&mut e, r#"'2024-02-30'::date"#, r#"ERR"#);
    ck(&mut e, r#"'99999'::int2"#, r#"ERR"#);
    ck(&mut e, r#"2147483648::int"#, r#"ERR"#);
    ck(&mut e, r#"'2147483648'::int"#, r#"ERR"#);
    ck(&mut e, r#"'256'::int2"#, r#"256"#);
    ck(&mut e, r#"32767::int2"#, r#"32767"#);
    ck(&mut e, r#"32768::int2"#, r#"ERR"#);
    ck(&mut e, r#"9223372036854775807::bigint"#, r#"9223372036854775807"#);
    ck(&mut e, r#"'9223372036854775808'::bigint"#, r#"ERR"#);
    ck(&mut e, r#"'1.239'::numeric(4,2)"#, r#"1.24"#);
    ck(&mut e, r#"'123.4'::numeric(3,0)"#, r#"123"#);
    ck(&mut e, r#"'1234.5'::numeric(3,0)"#, r#"ERR"#);
    ck(&mut e, r#"1.5::int"#, r#"2"#);
    ck(&mut e, r#"2.5::int"#, r#"3"#);
    ck(&mut e, r#"-1.5::int"#, r#"-2"#);
    ck(&mut e, r#"-2.5::int"#, r#"-3"#);
    ck(&mut e, r#"0.5::int"#, r#"1"#);
    ck(&mut e, r#"3.5::int"#, r#"4"#);
    ck(&mut e, r#"1.5::float8::int"#, r#"2"#);
    ck(&mut e, r#"1 = 1.0"#, r#"true"#);
    ck(&mut e, r#"'2024-01-01'::date = '2024-01-01'"#, r#"true"#);
    ck(&mut e, r#"'1' || 2"#, r#"12"#);
    ck(&mut e, r#"1.0 = 1"#, r#"true"#);
    ck(&mut e, r#"CASE WHEN true THEN 1 ELSE 2.5 END"#, r#"1"#);
    ck(&mut e, r#"COALESCE(1, 2.5)"#, r#"1"#);
    ck(&mut e, r#"greatest(1, 2.5)"#, r#"2.5"#);
    ck(&mut e, r#"least(1, 2.5)"#, r#"1"#);
    ck(&mut e, r#"'{"a": 1}'::jsonb::text"#, r#"{"a": 1}"#);
    ck(&mut e, r#"'\x41'::bytea"#, r#"\x41"#);
    ck(&mut e, r#"'A'::bytea"#, r#"\x41"#);
    ck(&mut e, r#"'\x41'::bytea::text"#, r#"\x41"#);
    ck(&mut e, r#"'12.5'::float8"#, r#"12.5"#);
    ck(&mut e, r#"'inf'::float8"#, r#"Infinity"#);
    ck(&mut e, r#"'nan'::float8"#, r#"NaN"#);
    ck(&mut e, r#"'  3.14  '::float8"#, r#"3.14"#);
    ck(&mut e, r#"1e3::int"#, r#"1000"#);
    ck(&mut e, r#"'1e3'::int"#, r#"ERR"#);
    ck(&mut e, r#"' 100'::bigint"#, r#"100"#);
    ck(&mut e, r#"'3.0'::int"#, r#"ERR"#);
    ck(&mut e, r#"100.999::numeric(4,1)"#, r#"101.0"#);
    ck(&mut e, r#"-0.0::int"#, r#"0"#);
    ck(&mut e, r#"'2.5'::numeric::int"#, r#"3"#);
    ck(&mut e, r#"2.5::numeric::int"#, r#"3"#);
    ck(&mut e, r#"'t'::boolean"#, r#"true"#);
    ck(&mut e, r#"127::int2"#, r#"127"#);
    ck(&mut e, r#"'true '::bool"#, r#"true"#);
    ck(&mut e, r#"1::numeric"#, r#"1"#);
    ck(&mut e, r#"1::numeric(5,2)"#, r#"1.00"#);
    ck(&mut e, r#"'+5'::int"#, r#"5"#);
    ck(&mut e, r#"'5.'::numeric"#, r#"5"#);
    ck(&mut e, r#"'.5'::numeric"#, r#"0.5"#);
    ck(&mut e, r#"'2024-01-15T10:30:00'::timestamp"#, r#"2024-01-15 10:30:00"#);
    ck(&mut e, r#"'10:30'::time"#, r#"10:30:00"#);
    ck(&mut e, r#"'2024-01-15 10'::timestamp"#, r#"ERR"#);
    ck(&mut e, r#"'2024-02-29'::date"#, r#"2024-02-29"#);
    ck(&mut e, r#"'2023-02-29'::date"#, r#"ERR"#);
    ck(&mut e, r#"'2024-04-31'::date"#, r#"ERR"#);
    ck(&mut e, r#"'2024-00-15'::date"#, r#"ERR"#);
    ck(&mut e, r#"'2024-01-00'::date"#, r#"ERR"#);
    ck(&mut e, r#"'  256  '::int2"#, r#"256"#);
    ck(&mut e, r#"'  3.14  '::float4"#, r#"3.14"#);
    ck(&mut e, r#"3.5::float8::int"#, r#"4"#);
    ck(&mut e, r#"-1.5::float8::int"#, r#"-2"#);
    ck(&mut e, r#"'-inf'::float8"#, r#"-Infinity"#);
    ck(&mut e, r#"'-inf'::float8::text"#, r#"-Infinity"#);
    ck(&mut e, r#"'10:30:00.5'::time"#, r#"10:30:00.5"#);
}
