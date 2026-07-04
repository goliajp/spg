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

/// int4 arithmetic overflow — PG errors "integer out of range" (int4
/// op int4 -> int4), it does NOT silently widen to bigint. Verified
/// live on PostgreSQL 18.4. Previously SPG promoted the result to
/// bigint, diverging from PG and from SPG's own `::int` cast (which
/// already errors on overflow).
#[test]
fn int_arithmetic_overflow() {
    let mut e = Engine::new();
    // overflowing int4 arithmetic errors (was silently -> bigint).
    ck(&mut e, r#"2147483647 + 1"#, r#"ERR"#);
    ck(&mut e, r#"2000000000 + 2000000000"#, r#"ERR"#);
    ck(&mut e, r#"2147483647 * 2"#, r#"ERR"#);
    // NOTE: `(-2147483648) - 1` also errors on PG (the int4-MIN literal folds
    // to int4, then MIN-1 overflows), but SPG lexes the magnitude 2147483648
    // as bigint so it takes the int8 path — a separate negative-literal-
    // folding gap tracked outside this int4-arithmetic fix.
    // in-range int4 arithmetic stays int4.
    ck(&mut e, r#"2147483647 + 0"#, r#"2147483647"#);
    ck(&mut e, r#"1000000 + 1000000"#, r#"2000000"#);
    // explicit bigint operands do widen and don't overflow int4.
    ck(&mut e, r#"2147483647::bigint + 1"#, r#"2147483648"#);
    // an oversized literal is already bigint (lexer), so no overflow.
    ck(&mut e, r#"9999999999 + 1"#, r#"10000000000"#);
}

/// bigint-vs-int mixed arithmetic operand order. The mixed arm of `arith`
/// used a `|`-merged pattern that always bound the Int operand on the left,
/// so `bigint - int` computed `int - bigint` and `bigint / int` computed
/// `int / bigint`. Commutative ops (+, *) were unaffected. PG18.4-verified.
#[test]
fn bigint_int_mixed_arithmetic() {
    let mut e = Engine::new();
    // bigint (LHS) op int (RHS) — order must be preserved.
    ck(&mut e, r#"5000000000 - 1"#, r#"4999999999"#);
    ck(&mut e, r#"5000000000 / 2"#, r#"2500000000"#);
    ck(&mut e, r#"5000000000 * 2"#, r#"10000000000"#);
    // int (LHS) op bigint (RHS) — already correct, kept as control.
    ck(&mut e, r#"1 - 5000000000"#, r#"-4999999999"#);
    ck(&mut e, r#"100 / 5000000000"#, r#"0"#);
    // int op int control.
    ck(&mut e, r#"10 - 3"#, r#"7"#);
}

/// PG string-function edge cases (negative/zero args, not-found, empty
/// search). Ground truth captured live from PostgreSQL 18.4. Two SPG
/// divergences fixed in the accompanying commit: substr(s, n, -1) now
/// raises "negative substring length not allowed" (was empty), and
/// ascii('') is 0 (was an error).
#[test]
fn string_function_edge_cases() {
    let mut e = Engine::new();
    ck(&mut e, r#"substr('hello', -1, 3)"#, r#"h"#);
    ck(&mut e, r#"substr('hello', 2, 0)"#, r#""#);
    ck(&mut e, r#"substr('hello', 2, -1)"#, r#"ERR"#);
    ck(&mut e, r#"split_part('a,b,c,d', ',', -1)"#, r#"d"#);
    ck(&mut e, r#"split_part('a,b', ',', 5)"#, r#""#);
    ck(&mut e, r#"left('hello', -2)"#, r#"hel"#);
    ck(&mut e, r#"right('hello', -2)"#, r#"llo"#);
    ck(&mut e, r#"lpad('hi', -1, 'x')"#, r#""#);
    ck(&mut e, r#"repeat('ab', -1)"#, r#""#);
    ck(&mut e, r#"strpos('hello', 'z')"#, r#"0"#);
    ck(&mut e, r#"replace('hello', '', 'x')"#, r#"hello"#);
    ck(&mut e, r#"btrim('xxhixx', 'x')"#, r#"hi"#);
    ck(&mut e, r#"translate('hello', 'l', '')"#, r#"heo"#);
    ck(&mut e, r#"ascii('')"#, r#"0"#);
}

/// PG timestamp/interval arithmetic. `timestamp - timestamp` returns an
/// interval justified to hours (30h -> `1 day 06:00:00`), fixed here from a
/// raw-microsecond BigInt. Ground truth captured live from PostgreSQL 18.4.
///
/// Remaining date divergences noted for follow-up (NOT fixed here):
///   * `date + interval` yields DATE in SPG, TIMESTAMP in PG.
///   * `age(ts, ts)` yields flat days in SPG, a symbolic mon/day interval in PG.
///   * `extract(epoch FROM ts)` renders integer in SPG, numeric `.000000` in PG.
#[test]
fn timestamp_interval_arithmetic() {
    let mut e = Engine::new();
    ck(&mut e, r#"'2024-01-02 12:00:00'::timestamp - '2024-01-01 06:00:00'::timestamp"#, r#"1 day 06:00:00"#);
    ck(&mut e, r#"'2024-01-01 00:00:00'::timestamp - '2024-01-01 00:00:00'::timestamp"#, r#"00:00:00"#);
    // controls that already matched PG.
    ck(&mut e, r#"'2024-01-31'::date + 1"#, r#"2024-02-01"#);
    ck(&mut e, r#"'2024-03-01'::date - '2024-02-01'::date"#, r#"29"#);
    ck(&mut e, r#"interval '2 days' - interval '1 day'"#, r#"1 day"#);
    ck(&mut e, r#"'2024-01-01'::date - 5"#, r#"2023-12-27"#);
}
/// Array + bytea function edge cases. `POSITION(bytea IN bytea)` lowered to
/// strpos and did a rendered-text search (finding the literal `\x..`
/// substring, i.e. 0) instead of PG's byte-level search. Fixed here.
/// Ground truth captured live from PostgreSQL 18.4.
#[test]
fn array_bytea_edge_cases() {
    let mut e = Engine::new();
    // the fix: byte-level POSITION.
    ck(&mut e, r#"position('\x34'::bytea in '\x1234'::bytea)"#, r#"2"#);
    ck(&mut e, r#"position('\xff'::bytea in '\x1234'::bytea)"#, r#"0"#);
    ck(&mut e, r#"position('\x1234'::bytea in '\x001234ab'::bytea)"#, r#"2"#);
    // controls that already matched PG.
    ck(&mut e, r#"array_length(ARRAY[]::int[], 1)"#, r#"<NULL>"#);
    ck(&mut e, r#"cardinality(ARRAY[]::int[])"#, r#"0"#);
    ck(&mut e, r#"(ARRAY[1,2,3])[5]"#, r#"<NULL>"#);
    ck(&mut e, r#"array_position(ARRAY[10,20,30], 99)"#, r#"<NULL>"#);
    ck(&mut e, r#"array_remove(ARRAY[1,2,2,3], 2)"#, r#"{1,3}"#);
    ck(&mut e, r#"'\x12'::bytea || '\x34'::bytea"#, r#"\x1234"#);
    ck(&mut e, r#"get_byte('\x1234'::bytea, 0)"#, r#"18"#);
    ck(&mut e, r#"substring('\x1234abcd'::bytea from 2 for 2)"#, r#"\x34ab"#);
}

/// inet builtins accept a real INET/CIDR value, not only its TEXT form.
/// (host/network/masklen/broadcast/family/netmask/hostmask errored on an
/// actual inet value.) PG18.4-verified.
#[test]
fn inet_functions_accept_inet_value() {
    let mut e = Engine::new();
    ck(&mut e, r#"host('192.168.1.5/24'::inet)"#, r#"192.168.1.5"#);
    ck(&mut e, r#"masklen('192.168.1.5/24'::inet)"#, r#"24"#);
    ck(&mut e, r#"network('192.168.1.5/24'::inet)"#, r#"192.168.1.0/24"#);
    ck(&mut e, r#"broadcast('192.168.1.5/24'::inet)"#, r#"192.168.1.255/24"#);
    ck(&mut e, r#"family('192.168.1.5'::inet)"#, r#"4"#);
    // TEXT form still works (legacy path).
    ck(&mut e, r#"host('192.168.1.5/24')"#, r#"192.168.1.5"#);
}

/// Range boolean operators @> (contains elem/range), <@ (contained by), &&
/// (overlap) — previously routed to json::contains and errored on ranges.
/// PG18.4-verified (bool renders as true/false via ::text).
#[test]
fn range_boolean_operators() {
    let mut e = Engine::new();
    ck(&mut e, r#"int4range(1,10) @> 5"#, r#"true"#);
    ck(&mut e, r#"int4range(1,10) @> 1"#, r#"true"#);
    ck(&mut e, r#"int4range(1,10) @> 10"#, r#"false"#);
    ck(&mut e, r#"int4range(1,10) @> 20"#, r#"false"#);
    ck(&mut e, r#"int4range(1,10) @> int4range(2,5)"#, r#"true"#);
    ck(&mut e, r#"int4range(1,10) @> int4range(5,15)"#, r#"false"#);
    ck(&mut e, r#"int4range(2,5) <@ int4range(1,10)"#, r#"true"#);
    ck(&mut e, r#"int4range(1,5) && int4range(4,10)"#, r#"true"#);
    ck(&mut e, r#"int4range(1,5) && int4range(5,10)"#, r#"false"#);
    ck(&mut e, r#"numrange(1,5,'[]') && numrange(5,10,'[]')"#, r#"true"#);
    ck(&mut e, r#"'empty'::int4range <@ int4range(1,10)"#, r#"true"#);
    ck(&mut e, r#"int4range(1,10) @> 'empty'::int4range"#, r#"true"#);
    ck(&mut e, r#"'empty'::int4range && int4range(1,10)"#, r#"false"#);
}

/// bit-string bitwise & / | over equal-length bit(n) values (byte-wise;
/// previously errored "cannot apply & to BitString"). PG18.4-verified.
#[test]
fn bit_string_bitwise_and_or() {
    let mut e = Engine::new();
    ck(&mut e, r#"B'1010' & B'0110'"#, r#"0010"#);
    ck(&mut e, r#"B'1010' | B'0110'"#, r#"1110"#);
    ck(&mut e, r#"B'11001010' & B'10101010'"#, r#"10001010"#);
    ck(&mut e, r#"B'11001010' | B'10101010'"#, r#"11101010"#);
    // differing lengths error.
    ck(&mut e, r#"B'1010' & B'101'"#, r#"ERR"#);
    // ~ NOT flips bits + re-zeros the padding.
    ck(&mut e, r#"~ B'1010'"#, r#"0101"#);
    ck(&mut e, r#"~ B'11001010'"#, r#"00110101"#);
    ck(&mut e, r#"~ B'111'"#, r#"000"#);
}

/// lower(anyrange) / upper(anyrange) return the range bounds (previously
/// errored "lower() needs text"). Text lower/upper are unchanged.
/// PG18.4-verified.
#[test]
fn range_lower_upper_functions() {
    let mut e = Engine::new();
    ck(&mut e, r#"lower(int4range(1,10))"#, r#"1"#);
    ck(&mut e, r#"upper(int4range(1,10))"#, r#"10"#);
    ck(&mut e, r#"lower('empty'::int4range)"#, r#"<NULL>"#);
    ck(&mut e, r#"upper('(,5)'::int4range)"#, r#"5"#);
    ck(&mut e, r#"lower('(,5)'::int4range)"#, r#"<NULL>"#);
    // text lower/upper still work.
    ck(&mut e, r#"lower('ABC')"#, r#"abc"#);
    ck(&mut e, r#"upper('abc')"#, r#"ABC"#);
}

/// Range `*` intersection returns the overlapping sub-range (empty if
/// disjoint) — previously errored. PG18.4-verified.
#[test]
fn range_intersection_operator() {
    let mut e = Engine::new();
    ck(&mut e, r#"int4range(1,10) * int4range(5,15)"#, r#"[5,10)"#);
    ck(&mut e, r#"int4range(1,10) * int4range(3,7)"#, r#"[3,7)"#);
    ck(&mut e, r#"int4range(1,5) * int4range(10,20)"#, r#"empty"#);
    ck(&mut e, r#"int4range(1,5) * int4range(5,10)"#, r#"empty"#);
    ck(&mut e, r#"int4range(1,10) * 'empty'::int4range"#, r#"empty"#);
}

/// Range `+` union returns the merged range (overlap/adjacent/contained) or
/// errors when the operands leave a gap. PG18.4-verified.
#[test]
fn range_union_operator() {
    let mut e = Engine::new();
    ck(&mut e, r#"int4range(1,5) + int4range(4,10)"#, r#"[1,10)"#);
    ck(&mut e, r#"int4range(1,5) + int4range(5,10)"#, r#"[1,10)"#);
    ck(&mut e, r#"int4range(1,10) + int4range(3,7)"#, r#"[1,10)"#);
    ck(&mut e, r#"'empty'::int4range + int4range(1,10)"#, r#"[1,10)"#);
    ck(&mut e, r#"int4range(1,10) + 'empty'::int4range"#, r#"[1,10)"#);
    // a gap between the operands is a contiguity error.
    ck(&mut e, r#"int4range(1,5) + int4range(10,20)"#, r#"ERR"#);
}

/// bit(n) << / >> k shift within the fixed-width window (previously errored).
/// PG18.4-verified: over-shift zeroes, negative count reverses direction.
#[test]
fn bit_string_shift_operators() {
    let mut e = Engine::new();
    ck(&mut e, r#"B'1010' << 1"#, r#"0100"#);
    ck(&mut e, r#"B'1010' >> 1"#, r#"0101"#);
    ck(&mut e, r#"B'11010' << 2"#, r#"01000"#);
    ck(&mut e, r#"B'1010' << 5"#, r#"0000"#);
    ck(&mut e, r#"B'1010' >> 5"#, r#"0000"#);
    ck(&mut e, r#"B'1010' << -1"#, r#"0101"#);
}

/// inet - inet returns the bigint count of addresses between them
/// (previously errored). PG18.4-verified.
#[test]
fn inet_minus_inet_bigint() {
    let mut e = Engine::new();
    ck(&mut e, r#"'192.168.1.5'::inet - '192.168.1.1'::inet"#, r#"4"#);
    ck(&mut e, r#"'192.168.1.1'::inet - '192.168.1.5'::inet"#, r#"-4"#);
    ck(&mut e, r#"'10.0.0.0'::inet - '10.0.0.0'::inet"#, r#"0"#);
    ck(&mut e, r#"'192.168.1.0'::inet - '192.168.0.0'::inet"#, r#"256"#);
}

/// array_fill(value, ARRAY[n]) builds a 1-D array of n copies (previously
/// errored). PG18.4-verified.
#[test]
fn array_fill_one_dim() {
    let mut e = Engine::new();
    ck(&mut e, r#"array_fill(7, ARRAY[3])"#, r#"{7,7,7}"#);
    ck(&mut e, r#"array_fill(0, ARRAY[0])"#, r#"{}"#);
    ck(&mut e, r#"array_fill(9::bigint, ARRAY[2])"#, r#"{9,9}"#);
    ck(&mut e, r#"array_fill('x', ARRAY[3])"#, r#"{x,x,x}"#);
}

/// age(ts, ts) is a calendar difference broken into months/days, not total
/// days (was `65 days`, should be `2 mons 5 days`). PG18.4-verified.
#[test]
fn age_calendar_breakdown() {
    let mut e = Engine::new();
    ck(&mut e, r#"age(timestamp '2024-03-15', timestamp '2024-01-10')"#, r#"2 mons 5 days"#);
    ck(&mut e, r#"age(timestamp '2024-03-05', timestamp '2024-01-10')"#, r#"1 mon 26 days"#);
    ck(&mut e, r#"age(timestamp '2024-03-01', timestamp '2024-01-01')"#, r#"2 mons"#);
    ck(&mut e, r#"age(timestamp '2024-01-10', timestamp '2024-03-15')"#, r#"-2 mons -5 days"#);
    ck(&mut e, r#"age(timestamp '2024-03-15 14:30:00', timestamp '2024-01-10 10:00:00')"#, r#"2 mons 5 days 04:30:00"#);
}

/// time - time returns the signed interval between them (was ERR). PG18.4.
#[test]
fn time_minus_time_interval() {
    let mut e = Engine::new();
    ck(&mut e, r#"'10:30:00'::time - '08:15:00'::time"#, r#"02:15:00"#);
    ck(&mut e, r#"'08:15:00'::time - '10:30:00'::time"#, r#"-02:15:00"#);
    ck(&mut e, r#"'12:00:00'::time - '12:00:00'::time"#, r#"00:00:00"#);
    ck(&mut e, r#"'23:59:59'::time - '00:00:00'::time"#, r#"23:59:59"#);
}


/// PG shows an explicit `+` on the time part of an interval when a preceding
/// date field is negative but the time is positive: `-1 days +02:00:00`.
/// PG18.4-verified.
#[test]
fn interval_mixed_sign_plus_render() {
    let mut e = Engine::new();
    ck(&mut e, r#"interval '1 day 2 hours'"#, r#"1 day 02:00:00"#);
    ck(&mut e, r#"interval '-1 day 2 hours'"#, r#"-1 days +02:00:00"#);
    ck(&mut e, r#"interval '1 day -2 hours'"#, r#"1 day -02:00:00"#);
    ck(&mut e, r#"interval '-1 day -2 hours'"#, r#"-1 days -02:00:00"#);
    ck(&mut e, r#"interval '-1 month -1 day -2 hours'"#, r#"-1 mons -1 days -02:00:00"#);
    ck(&mut e, r#"interval '1 month -2 hours'"#, r#"1 mon -02:00:00"#);
    ck(&mut e, r#"interval '-2 hours'"#, r#"-02:00:00"#);
}

/// Fractional interval units cascade to the next-finer field, PG-style
/// (was ERR). PG18.4-verified.
#[test]
fn interval_fractional_units() {
    let mut e = Engine::new();
    ck(&mut e, r#"interval '1.5 hours'"#, r#"01:30:00"#);
    ck(&mut e, r#"interval '2.5 seconds'"#, r#"00:00:02.5"#);
    ck(&mut e, r#"interval '0.25 hours'"#, r#"00:15:00"#);
    ck(&mut e, r#"interval '1.5 days'"#, r#"1 day 12:00:00"#);
    ck(&mut e, r#"interval '1.5 weeks'"#, r#"10 days 12:00:00"#);
    ck(&mut e, r#"interval '1.5 months'"#, r#"1 mon 15 days"#);
    ck(&mut e, r#"interval '1.5 years'"#, r#"1 year 6 mons"#);
    ck(&mut e, r#"interval '1.5 months 2.5 hours'"#, r#"1 mon 15 days 02:30:00"#);
}

/// PG `TYPE 'literal'` typed literals for the scalar type family (== the
/// `'literal'::TYPE` cast). Only date/timestamp/timestamptz/interval worked
/// before; time/bool/int/numeric/uuid errored. PG18.4-verified.
#[test]
fn typed_literals_scalar_family() {
    let mut e = Engine::new();
    ck(&mut e, r#"time '10:30:00'"#, r#"10:30:00"#);
    ck(&mut e, r#"bool 'true'"#, r#"true"#);
    ck(&mut e, r#"int '42'"#, r#"42"#);
    ck(&mut e, r#"bigint '9000000000'"#, r#"9000000000"#);
    ck(&mut e, r#"numeric '3.14'"#, r#"3.14"#);
    ck(&mut e, r#"uuid '00000000-0000-0000-0000-000000000001'"#, r#"00000000-0000-0000-0000-000000000001"#);
    // regression: the datetime typed literals still work.
    ck(&mut e, r#"date '2024-01-15'"#, r#"2024-01-15"#);
    ck(&mut e, r#"timestamp '2024-01-15 10:30:00'"#, r#"2024-01-15 10:30:00"#);
}

/// PG's total float order: NaN == NaN and NaN > every number, so scalar
/// float comparisons never error on NaN (were `ERR`). PG18.4-verified.
#[test]
fn float_nan_comparison_total_order() {
    let mut e = Engine::new();
    ck(&mut e, r#"'NaN'::float8 = 'NaN'::float8"#, r#"true"#);
    ck(&mut e, r#"'NaN'::float8 > 1"#, r#"true"#);
    ck(&mut e, r#"'NaN'::float8 >= 'Infinity'::float8"#, r#"true"#);
    ck(&mut e, r#"1 < 'NaN'::float8"#, r#"true"#);
    ck(&mut e, r#"'NaN'::float8 <> 1"#, r#"true"#);
    ck(&mut e, r#"'NaN'::float8 = 1"#, r#"false"#);
    // sanity: ordinary float comparison unaffected.
    ck(&mut e, r#"1.5::float8 < 2.5::float8"#, r#"true"#);
}

/// to_char `PR` accounting-negative notation: negatives wrap in angle
/// brackets (no minus), non-negatives get a trailing space (where `>` sits),
/// FM strips the padding. Was rendered with a plain `-`. PG18.4-verified.
#[test]
fn to_char_pr_notation() {
    let mut e = Engine::new();
    ck(&mut e, r#"to_char(-1234.5, 'FM9999.00PR')"#, r#"<1234.50>"#);
    ck(&mut e, r#"to_char(1234.5, 'FM9999.00PR')"#, r#"1234.50"#);
    ck(&mut e, r#"to_char(-5, '999PR')"#, r#"  <5>"#);
    ck(&mut e, r#"to_char(5, '999PR')"#, r#"   5 "#);
    ck(&mut e, r#"to_char(0, 'FM9PR')"#, r#"0"#);
}

/// regexp_replace honours the `i` (case-insensitive) flag (was ignored — no
/// match). PG18.4-verified. Backreferences in the replacement remain a
/// separate deferred gap (needs regex capture-group support).
#[test]
fn regexp_replace_case_insensitive_flag() {
    let mut e = Engine::new();
    ck(&mut e, r#"regexp_replace('HELLO', 'hello', 'x', 'i')"#, r#"x"#);
    ck(&mut e, r#"regexp_replace('FooBar', 'o', 'O', 'gi')"#, r#"FOOBar"#);
    ck(&mut e, r#"regexp_replace('ABC', 'abc', 'z', 'i')"#, r#"z"#);
    // case-sensitive default still doesn't match.
    ck(&mut e, r#"regexp_replace('HELLO', 'hello', 'x')"#, r#"HELLO"#);
    // g flag unaffected.
    ck(&mut e, r#"regexp_replace('hello world', 'o', 'O', 'g')"#, r#"hellO wOrld"#);
}

/// The whole regexp family honours the `i` (case-insensitive) flag at its
/// per-function flags-argument position (was ignored family-wide). PG18.4-verified.
#[test]
fn regexp_family_case_insensitive_flag() {
    let mut e = Engine::new();
    ck(&mut e, r#"regexp_count('AbAb', 'a', 1, 'i')"#, r#"2"#);
    ck(&mut e, r#"regexp_substr('HELLO', 'l+', 1, 1, 'i')"#, r#"LL"#);
    ck(&mut e, r#"regexp_instr('xxAB', 'ab', 1, 1, 0, 'i')"#, r#"3"#);
    ck(&mut e, r#"regexp_match('HELLO', 'ell', 'i')"#, r#"{ELL}"#);
    // case-sensitive default still no match.
    ck(&mut e, r#"regexp_count('AbAb', 'a')"#, r#"0"#);
    ck(&mut e, r#"regexp_substr('HELLO', 'l+', 1, 1)"#, r#"<NULL>"#);
}

/// The `%` operator is truncated-division remainder (sign of the dividend),
/// matching PG / C / the mod() function — was Euclidean (always non-negative),
/// so `-5 % 3` returned 1 instead of -2. PG18.4-verified.
#[test]
fn modulo_operator_sign_of_dividend() {
    let mut e = Engine::new();
    ck(&mut e, r#"(-5 % 3)"#, r#"-2"#);
    ck(&mut e, r#"(5 % -3)"#, r#"2"#);
    ck(&mut e, r#"(-5 % -3)"#, r#"-2"#);
    ck(&mut e, r#"(5 % 3)"#, r#"2"#);
    // mod() function already matched and stays consistent.
    ck(&mut e, r#"mod(-5, 3)"#, r#"-2"#);
    ck(&mut e, r#"mod(-5, 3) = (-5 % 3)"#, r#"true"#);
}

/// `expr::float8[]` (and float4[]/real[]) cast target — was ERR: the parser
/// resolved it to Named("float8_array") but the type resolver didn't know that
/// alias and coerce_value had no non-empty TEXT[]→FLOAT[] arm. PG18.4-verified.
#[test]
fn cast_to_float_array() {
    let mut e = Engine::new();
    ck(&mut e, r#"(ARRAY[1.5,2.5]::float8[])::text"#, r#"{1.5,2.5}"#);
    ck(&mut e, r#"(ARRAY[1,2]::float8[])::text"#, r#"{1,2}"#);
    ck(&mut e, r#"(ARRAY[1.5,2.5]::float4[])::text"#, r#"{1.5,2.5}"#);
    ck(&mut e, r#"(ARRAY[10,20]::int[])::text"#, r#"{10,20}"#);
}

/// The rest of the array cast-target family (`::bool[]`, `::numeric[]`,
/// `::date[]`, `::timestamp[]`, `::uuid[]`, `::varchar[]`) — was ERR: only the
/// empty-array arm existed, so non-empty literals fell through. Each element is
/// now parsed via the scalar coerce path. PG18.4-verified.
#[test]
fn cast_to_typed_array_family() {
    let mut e = Engine::new();
    ck(&mut e, r#"(ARRAY[true,false]::bool[])::text"#, r#"{t,f}"#);
    ck(&mut e, r#"(ARRAY[1.5,2.5]::numeric[])::text"#, r#"{1.5,2.5}"#);
    ck(&mut e, r#"(ARRAY['2024-01-01','2024-02-01']::date[])::text"#, r#"{2024-01-01,2024-02-01}"#);
    ck(&mut e, r#"(ARRAY['a','b']::varchar[])::text"#, r#"{a,b}"#);
    ck(&mut e, r#"(ARRAY['a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11']::uuid[])::text"#, r#"{a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11}"#);
    // controls that already worked.
    ck(&mut e, r#"(ARRAY[1,2]::smallint[])::text"#, r#"{1,2}"#);
    ck(&mut e, r#"(ARRAY[1,2]::int[])::text"#, r#"{1,2}"#);
}

/// PG: `date ± interval` always yields TIMESTAMP (the interval may carry a
/// time component), rendered at midnight when it has no sub-day part. SPG kept
/// it a DATE when the interval was pure-days. PG18.4-verified. (`date ± integer`
/// stays DATE — a different operator.)
#[test]
fn date_interval_promotes_to_timestamp() {
    let mut e = Engine::new();
    ck(&mut e, r#"date '2024-03-01' - interval '1 day'"#, r#"2024-02-29 00:00:00"#);
    ck(&mut e, r#"date '2024-03-01' + interval '1 day'"#, r#"2024-03-02 00:00:00"#);
    ck(&mut e, r#"date '2024-03-01' + interval '2 hours'"#, r#"2024-03-01 02:00:00"#);
    ck(&mut e, r#"date '2024-03-01' - interval '1 month'"#, r#"2024-02-01 00:00:00"#);
    // date ± integer stays a date.
    ck(&mut e, r#"date '2024-03-01' + 5"#, r#"2024-03-06"#);
    ck(&mut e, r#"date '2024-03-01' - 5"#, r#"2024-02-25"#);
}

/// to_char timestamp format: double-quoted text is a literal (quotes stripped),
/// e.g. `HH24"h"MI"m"` → `14h30m`, `YYYY"年"MM"月"` → `2024年03月`. Was emitted
/// with the quotes verbatim. PG18.4-verified.
#[test]
fn to_char_ts_quoted_literal() {
    let mut e = Engine::new();
    ck(&mut e, r#"to_char(timestamp '2024-03-05 14:30:45', 'HH24"h"MI"m"')"#, r#"14h30m"#);
    ck(&mut e, r#"to_char(timestamp '2024-03-05 14:30:45', 'YYYY"年"MM"月"DD"日"')"#, r#"2024年03月05日"#);
    ck(&mut e, r#"to_char(timestamp '2024-03-05 14:30:45', '"at" HH24:MI')"#, r#"at 14:30"#);
    // control: no quotes unaffected.
    ck(&mut e, r#"to_char(timestamp '2024-03-05 14:30:45', 'YYYY-MM-DD')"#, r#"2024-03-05"#);
}

/// encode 'escape' format + decode returns bytea (was Text, which broke
/// `decode(...)::text` rendering and non-UTF-8 decode round-trips). PG18.4-verified.
#[test]
fn encode_escape_and_decode_bytea() {
    let mut e = Engine::new();
    ck(&mut e, r#"encode('abc'::bytea, 'escape')"#, r#"abc"#);
    // decode returns bytea → renders as \xHEX (was `abc`).
    ck(&mut e, r#"(decode('YWJj', 'base64'))::text"#, r#"\x616263"#);
    ck(&mut e, r#"(decode('616263', 'hex'))::text"#, r#"\x616263"#);
    // non-UTF-8 decode round-trips through encode (was ERR).
    ck(&mut e, r#"encode(decode('deadbeef','hex'),'base64')"#, r#"3q2+7w=="#);
    ck(&mut e, r#"encode(decode('deadbeef','hex'),'hex')"#, r#"deadbeef"#);
    // controls unaffected.
    ck(&mut e, r#"encode('abc'::bytea, 'hex')"#, r#"616263"#);
    ck(&mut e, r#"encode('abc'::bytea, 'base64')"#, r#"YWJj"#);
}

/// Statistical aggregates (stddev/variance/percentile_cont/corr) on a NUMERIC
/// column — the value→f64 conversion was missing the Numeric arm, so they
/// errored or returned NULL. PG18.4-verified.
#[test]
fn stat_agg_on_numeric() {
    use spg_engine::QueryResult;
    let mut e = Engine::new();
    e.execute("CREATE TABLE s(v numeric)").unwrap();
    e.execute("INSERT INTO s VALUES (1),(2),(3),(4),(5)").unwrap();
    let q = |e: &mut Engine, sql: &str| -> String {
        match e.execute(sql) {
            Ok(QueryResult::Rows { rows, .. }) => match &rows[0].values[0] {
                spg_storage::Value::Null => "<NULL>".into(),
                spg_storage::Value::Text(t) => t.to_string(),
                other => format!("{other:?}"),
            },
            Err(err) => format!("ERR:{err:?}"),
            _ => "<x>".into(),
        }
    };
    // The core fix: these now COMPUTE on a NUMERIC column instead of erroring
    // (stddev/variance) or returning NULL (percentile_cont/corr). Values are
    // numerically PG-correct; the trailing-zero padding on round(float,4)
    // (2.5 vs PG's 2.5000) is the separate numeric-scale deferral.
    assert_eq!(q(&mut e, "SELECT round(stddev(v),4)::text FROM s"), "1.5811");
    assert_eq!(q(&mut e, "SELECT round(variance(v),4)::text FROM s"), "2.5");
    assert_eq!(q(&mut e, "SELECT round(var_pop(v),4)::text FROM s"), "2");
    assert_eq!(q(&mut e, "SELECT percentile_cont(0.5) WITHIN GROUP (ORDER BY v)::text FROM s"), "3");
    assert_eq!(q(&mut e, "SELECT percentile_cont(0.25) WITHIN GROUP (ORDER BY v)::text FROM s"), "2");
    assert_eq!(q(&mut e, "SELECT corr(v, v)::text FROM s"), "1");
}

/// mod() accepts NUMERIC / FLOAT operands, not just integers — PG returns an
/// exact numeric remainder (`mod(7.5, 2.0) = 1.5`). Was "needs integer" ERR.
/// PG18.4-verified.
#[test]
fn mod_on_numeric() {
    let mut e = Engine::new();
    ck(&mut e, r#"mod(7.5::numeric, 2.0::numeric)"#, r#"1.5"#);
    ck(&mut e, r#"mod(10.5::numeric, 3::numeric)"#, r#"1.5"#);
    ck(&mut e, r#"mod((-7.5)::numeric, 2.0::numeric)"#, r#"-1.5"#);
    ck(&mut e, r#"mod(7.5::float8, 2.0::float8)"#, r#"1.5"#);
    // integer mod unaffected.
    ck(&mut e, r#"mod(7, 3)"#, r#"1"#);
    ck(&mut e, r#"mod(-5, 3)"#, r#"-2"#);
}

/// Range `-` difference operator (`+` union and `*` intersection already
/// worked). Errors when the removal would split the range in two. PG18.4-verified.
#[test]
fn range_difference_operator() {
    let mut e = Engine::new();
    ck(&mut e, r#"(int4range(1,10) - int4range(5,15))::text"#, r#"[1,5)"#);
    ck(&mut e, r#"(int4range(1,5) - int4range(4,10))::text"#, r#"[1,4)"#);
    ck(&mut e, r#"(int4range(5,15) - int4range(1,10))::text"#, r#"[10,15)"#);
    ck(&mut e, r#"(int4range(1,10) - int4range(1,10))::text"#, r#"empty"#);
    ck(&mut e, r#"(int4range(1,10) - int4range(20,30))::text"#, r#"[1,10)"#);
    // control: union/intersect still work.
    ck(&mut e, r#"(int4range(1,5) + int4range(4,10))::text"#, r#"[1,10)"#);
    ck(&mut e, r#"(int4range(1,10) * int4range(5,15))::text"#, r#"[5,10)"#);
}

/// tsquery `&&` (AND) and `||` (OR) operators — the function forms existed but
/// the operators errored (`&&`) or hit text concat (`||` → 'quick''fox'). Now
/// combine the two tsquery ASTs. PG18.4-verified.
#[test]
fn tsquery_bool_operators() {
    let mut e = Engine::new();
    ck(&mut e, r#"(to_tsquery('quick') || to_tsquery('fox'))::text"#, r#"'quick' | 'fox'"#);
    ck(&mut e, r#"(to_tsquery('quick') && to_tsquery('fox'))::text"#, r#"'quick' & 'fox'"#);
    // combined AST evaluates against a tsvector correctly.
    ck(&mut e, r#"to_tsvector('the quick brown fox') @@ (to_tsquery('cat') || to_tsquery('fox'))"#, r#"true"#);
    ck(&mut e, r#"to_tsvector('the quick brown fox') @@ (to_tsquery('cat') && to_tsquery('fox'))"#, r#"false"#);
    // control: tsvector || tsvector concat still works (SPG's default Simple
    // config does not stem, unlike PG's english default — a separate config
    // matter, not a bug in the operator).
    ck(&mut e, r#"(to_tsvector('cats') || to_tsvector('dogs'))::text"#, r#"'cats':1 'dogs':2"#);
}

/// MONEY arithmetic (money±money, money×/÷ number, money/money ratio) and
/// money::numeric cast — literal/render/compare already worked. PG18.4-verified.
#[test]
fn money_arithmetic() {
    let mut e = Engine::new();
    ck(&mut e, r#"('$100.00'::money + '$50.00'::money)::text"#, r#"$150.00"#);
    ck(&mut e, r#"('$100.00'::money - '$30.00'::money)::text"#, r#"$70.00"#);
    ck(&mut e, r#"('$100.00'::money * 2)::text"#, r#"$200.00"#);
    ck(&mut e, r#"(2 * '$100.00'::money)::text"#, r#"$200.00"#);
    ck(&mut e, r#"('$100.00'::money / 4)::text"#, r#"$25.00"#);
    ck(&mut e, r#"('$100.00'::money / '$50.00'::money)::text"#, r#"2"#);
    ck(&mut e, r#"('$100.00'::money::numeric)::text"#, r#"100.00"#);
    // controls: literal, render, compare unchanged.
    ck(&mut e, r#"('$1,234.56'::money)::text"#, r#"$1,234.56"#);
    ck(&mut e, r#"('$100.00'::money > '$50.00'::money)::text"#, r#"true"#);
}

/// bit-string length/bit_length + bit→integer cast. Postfix `::` on a bare
/// `B'..'` literal previously errored at parse time. PG18.4-verified.
#[test]
fn bit_string_length_and_int_cast() {
    let mut e = Engine::new();
    ck(&mut e, r#"length(B'10101')::text"#, r#"5"#);
    ck(&mut e, r#"bit_length(B'1010')::text"#, r#"4"#);
    ck(&mut e, r#"(B'1010'::int)::text"#, r#"10"#);
    ck(&mut e, r#"(B'11111111'::int)::text"#, r#"255"#);
    ck(&mut e, r#"(B'1010'::bigint)::text"#, r#"10"#);
    ck(&mut e, r#"(X'1F'::int)::text"#, r#"31"#);
    // controls: bitwise ops + equality unchanged.
    ck(&mut e, r#"(B'1010' & B'1100')::text"#, r#"1000"#);
    ck(&mut e, r#"(B'1010' = B'1010')::text"#, r#"true"#);
}

/// trunc(macaddr) zeros the last 3 bytes; macaddr→macaddr8 inserts ff:fe.
/// macaddr comparison/equality already worked. PG18.4-verified.
#[test]
fn macaddr_trunc_and_widen() {
    let mut e = Engine::new();
    ck(&mut e, r#"trunc('08:00:2b:01:02:03'::macaddr)::text"#, r#"08:00:2b:00:00:00"#);
    ck(&mut e, r#"('08:00:2b:01:02:03'::macaddr::macaddr8)::text"#, r#"08:00:2b:ff:fe:01:02:03"#);
    // controls: comparison + equality unchanged.
    ck(&mut e, r#"('08:00:2b:01:02:03'::macaddr < '08:00:2b:01:02:04'::macaddr)::text"#, r#"true"#);
    ck(&mut e, r#"('08:00:2b:01:02:03'::macaddr = '08:00:2b:01:02:03'::macaddr)::text"#, r#"true"#);
}

/// point <-> point Euclidean distance + point ± point translation. point
/// literal/render already worked; box/circle/line remain unimplemented.
/// PG18.4-verified.
#[test]
fn point_distance_and_arith() {
    let mut e = Engine::new();
    ck(&mut e, r#"('(0,0)'::point <-> '(3,4)'::point)::text"#, r#"5"#);
    ck(&mut e, r#"('(1,2)'::point + '(3,4)'::point)::text"#, r#"(4,6)"#);
    ck(&mut e, r#"('(4,6)'::point - '(1,2)'::point)::text"#, r#"(3,4)"#);
    ck(&mut e, r#"('(1,1)'::point <-> '(1,1)'::point)::text"#, r#"0"#);
    // control: point literal/render unchanged.
    ck(&mut e, r#"('(1,2)'::point)::text"#, r#"(1,2)"#);
}

/// set_masklen(inet/cidr, n) + abbrev(cidr). host/network/masklen/inet-inet
/// already worked. PG18.4-verified.
#[test]
fn inet_set_masklen_and_abbrev() {
    let mut e = Engine::new();
    ck(&mut e, r#"set_masklen('192.168.1.5/24'::inet, 16)::text"#, r#"192.168.1.5/16"#);
    ck(&mut e, r#"abbrev('192.168.1.0/24'::cidr)"#, r#"192.168.1/24"#);
    ck(&mut e, r#"abbrev('10.0.0.0/8'::cidr)"#, r#"10/8"#);
    // controls: host/network/masklen unchanged.
    ck(&mut e, r#"host('192.168.1.5/24'::inet)"#, r#"192.168.1.5"#);
    ck(&mut e, r#"masklen('192.168.1.5/24'::inet)::text"#, r#"24"#);
}

/// BUG: numrange @> a float element returned false (bound_cmp fell through to
/// Equal for mixed numeric/float, wrongly excluding the exclusive upper).
/// PG18.4-verified.
#[test]
fn numrange_contains_float_element() {
    let mut e = Engine::new();
    ck(&mut e, r#"(numrange(1.5,3.5) @> 2.5)::text"#, r#"true"#);
    ck(&mut e, r#"(numrange(1.5,3.5) @> 3.5)::text"#, r#"false"#); // exclusive upper
    ck(&mut e, r#"(numrange(1.5,3.5) @> 1.5)::text"#, r#"true"#);  // inclusive lower
    ck(&mut e, r#"(numrange(1.5,3.5) @> 0.5)::text"#, r#"false"#);
    ck(&mut e, r#"(2.5 <@ numrange(1.5,3.5))::text"#, r#"true"#);
    // control: integer range containment unchanged.
    ck(&mut e, r#"(int4range(1,10) @> 5)::text"#, r#"true"#);
}

/// BUG: INSERT / assignment of a text literal into an INTERVAL column failed
/// with a type mismatch — coerce_value had no Text→Interval arm, though the
/// `::interval` cast worked. Common for pg_dump reloads and ORMs. PG18.4-verified.
#[test]
fn insert_text_into_interval_column() {
    let mut e = Engine::new();
    let row = |e: &mut Engine, q: &str| -> String {
        match e.execute(q) {
            Ok(spg_engine::QueryResult::Rows { rows, .. }) if !rows.is_empty() => {
                format!("{:?}", rows[0].values[0])
            }
            other => format!("{other:?}"),
        }
    };
    e.execute("CREATE TABLE ivt (a int, iv interval)").unwrap();
    e.execute("INSERT INTO ivt VALUES (1,'1 day'),(2,'2 hours 30 minutes'),(3,'1 mon 5 days')")
        .expect("text literals coerce into an interval column");
    assert_eq!(row(&mut e, "SELECT iv::text FROM ivt WHERE a=1"), "Text(\"1 day\")");
    assert_eq!(row(&mut e, "SELECT iv::text FROM ivt WHERE a=2"), "Text(\"02:30:00\")");
    assert_eq!(row(&mut e, "SELECT iv::text FROM ivt WHERE a=3"), "Text(\"1 mon 5 days\")");
    e.execute("UPDATE ivt SET iv='45 seconds' WHERE a=1").unwrap();
    assert_eq!(row(&mut e, "SELECT iv::text FROM ivt WHERE a=1"), "Text(\"00:00:45\")");
    assert!(e.execute("INSERT INTO ivt VALUES (4,'not an interval')").is_err());
}

/// BUG: `CREATE TABLE t (x bit(4))` failed to parse — the column-type parser
/// accepted bare `bit`/`varbit` but not the `(N)` length modifier (nor
/// `bit varying`). SPG carries bit width in the value, so the typmod is
/// accepted and ignored. PG18.4-verified.
#[test]
fn create_table_bit_typmod() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE bc (a bit(4), b varbit(8), c bit varying(16), d bit)")
        .expect("bit/varbit column types accept a length modifier");
    e.execute("INSERT INTO bc VALUES ('1010','11110000','1','0')").unwrap();
    let row = |e: &mut Engine, q: &str| -> String {
        match e.execute(q) {
            Ok(spg_engine::QueryResult::Rows { rows, .. }) if !rows.is_empty() => {
                format!("{:?}", rows[0].values[0])
            }
            other => format!("{other:?}"),
        }
    };
    assert_eq!(row(&mut e, "SELECT a::text FROM bc"), "Text(\"1010\")");
    assert_eq!(row(&mut e, "SELECT b::text FROM bc"), "Text(\"11110000\")");
}

/// pg_dump-compat column type spellings that previously failed to parse at
/// CREATE TABLE: precision on time/timestamptz, precision before WITH TIME
/// ZONE, interval field qualifiers, and `character varying`. PG18.4-verified.
#[test]
fn create_table_datetime_typmods() {
    let mut e = Engine::new();
    e.execute(
        "CREATE TABLE dtm (a time(3), b timestamptz(6), c timestamp(3) with time zone, \
         d time(3) with time zone, f interval day to second, g interval(3), \
         h character varying(20), i character(4), j character varying)",
    )
    .expect("pg_dump-canonical datetime/character column typmods parse");
    e.execute("INSERT INTO dtm (h) VALUES ('hello')").unwrap();
    let r = e.execute("SELECT h FROM dtm");
    assert!(matches!(r, Ok(spg_engine::QueryResult::Rows { .. })));
}

/// sum(interval) / avg(interval) — the aggregate accumulator had no interval
/// state and rejected them. avg uses PG interval_div (month/day remainders
/// spill into the time field). PG18.4-verified.
#[test]
fn agg_interval_sum_avg() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE ait (iv interval)").unwrap();
    e.execute("INSERT INTO ait VALUES ('1 day'),('2 hours'),('30 minutes')").unwrap();
    let row = |e: &mut Engine, q: &str| -> String {
        match e.execute(q) {
            Ok(spg_engine::QueryResult::Rows { rows, .. }) if !rows.is_empty() => {
                format!("{:?}", rows[0].values[0])
            }
            other => format!("{other:?}"),
        }
    };
    assert_eq!(row(&mut e, "SELECT sum(iv)::text FROM ait"), "Text(\"1 day 02:30:00\")");
    assert_eq!(row(&mut e, "SELECT avg(iv)::text FROM ait"), "Text(\"08:50:00\")");
    assert_eq!(row(&mut e, "SELECT max(iv)::text FROM ait"), "Text(\"1 day\")");
    // month component: sum('1 mon','2 mons') = 3 mons; avg = 1 mon 15 days.
    e.execute("CREATE TABLE ait2 (iv interval)").unwrap();
    e.execute("INSERT INTO ait2 VALUES ('1 mon'),('2 mons')").unwrap();
    assert_eq!(row(&mut e, "SELECT sum(iv)::text FROM ait2"), "Text(\"3 mons\")");
    assert_eq!(row(&mut e, "SELECT avg(iv)::text FROM ait2"), "Text(\"1 mon 15 days\")");
}

/// Range `<<` (strictly left of) and `>>` (strictly right of). Boundary rule:
/// at an equal touching value they must not both be inclusive. PG18.4-verified.
#[test]
fn range_strictly_left_right() {
    let mut e = Engine::new();
    ck(&mut e, "(int4range(1,10) << int4range(20,30))::text", "true");
    ck(&mut e, "(int4range(1,10) << int4range(10,20))::text", "true"); // excl upper meets incl lower
    ck(&mut e, "(int4range(1,10) << int4range(5,20))::text", "false"); // overlap
    ck(&mut e, "('[1,10]'::int4range << '[10,20]'::int4range)::text", "false"); // both incl at 10
    ck(&mut e, "(int4range(20,30) >> int4range(1,10))::text", "true");
    ck(&mut e, "(int4range(1,10) >> int4range(20,30))::text", "false");
    ck(&mut e, "(numrange(1.5,3.5) << numrange(3.5,5.5))::text", "true");
    // control: overlap/union unaffected.
    ck(&mut e, "(int4range(1,5) && int4range(4,10))::text", "true");
}

/// range_merge on typed range values (was TEXT-only). The smallest range
/// containing both, spanning any gap. PG18.4-verified.
#[test]
fn range_merge_typed() {
    let mut e = Engine::new();
    ck(&mut e, "range_merge(int4range(1,5), int4range(8,10))::text", "[1,10)");
    ck(&mut e, "range_merge(int4range(1,5), int4range(3,8))::text", "[1,8)");
    ck(&mut e, "range_merge(numrange(1.5,3.5), numrange(5.5,7.5))::text", "[1.5,7.5)");
    ck(&mut e, "range_merge(int4range(1,5), 'empty'::int4range)::text", "[1,5)");
    ck(&mut e, "range_merge('[1,10]'::int4range, '[3,20]'::int4range)::text", "[1,21)"); // int4range canonicalizes []→[)
    // control: union of contiguous ranges still works.
    ck(&mut e, "(int4range(1,5) + int4range(4,10))::text", "[1,10)");
}

/// `#` bitwise XOR — was desugared to (a|b)-(a&b) (fine for ints, undefined
/// for bit strings). Now a real BinOp::BitXor. PG18.4-verified.
#[test]
fn bit_xor_operator() {
    let mut e = Engine::new();
    ck(&mut e, "(B'1100' # B'1010')::text", "0110");
    ck(&mut e, "(B'11110000' # B'00001111')::text", "11111111");
    ck(&mut e, "(5 # 3)::text", "6");     // integer XOR unchanged
    ck(&mut e, "(12 # 10)::text", "6");
    ck(&mut e, "(255 # 0)::text", "255");
}

/// Range `-|-` "is adjacent to" — new lexer token + operator. PG18.4-verified.
#[test]
fn range_adjacent_operator() {
    let mut e = Engine::new();
    ck(&mut e, "(int4range(1,5) -|- int4range(5,10))::text", "true");
    ck(&mut e, "(int4range(1,5) -|- int4range(6,10))::text", "false"); // gap
    ck(&mut e, "(int4range(1,5) -|- int4range(4,10))::text", "false"); // overlap
    ck(&mut e, "('[1,5]'::int4range -|- '[6,10]'::int4range)::text", "true");
    ck(&mut e, "(numrange(1.0,5.0) -|- numrange(5.0,9.0))::text", "true");
    // control: subtraction still works (a-b), not mislexed as -|-.
    ck(&mut e, "(10 - 3)::text", "7");
}

/// tsquery `!!` prefix negation — new lexer token, lowers to tsquery_not.
/// PG18.4-verified.
#[test]
fn tsquery_double_bang() {
    let mut e = Engine::new();
    ck(&mut e, "(!! 'cat'::tsquery)::text", "!'cat'");
    ck(&mut e, "(!! 'cat & dog'::tsquery)::text", "!( 'cat' & 'dog' )");
    ck(&mut e, "('cat'::tsquery && !! 'dog'::tsquery)::text", "'cat' & !'dog'");
    // control: != still works (not mislexed as !!).
    ck(&mut e, "(1 != 2)::text", "true");
}

/// point(x, y) constructor + point `*` (complex multiply) / `/` (complex
/// divide). Distance `<->` and `+`/`-` already worked. PG18.4-verified.
#[test]
fn point_constructor_and_mul() {
    let mut e = Engine::new();
    ck(&mut e, "point(1,2)::text", "(1,2)");
    ck(&mut e, "(point(0,0) <-> point(3,4))::text", "5");
    ck(&mut e, "(point(1,2) + point(3,4))::text", "(4,6)");
    ck(&mut e, "(point(2,3) * point(1,0))::text", "(2,3)");
    ck(&mut e, "(point(1,1) * point(0,1))::text", "(-1,1)"); // i*(1+i) = -1+i
}

/// lseg text input accepts the bare `(x1,y1),(x2,y2)` spelling, not just the
/// bracketed `[...]` form; output is always bracketed. PG18.4-verified.
#[test]
fn lseg_bare_input() {
    let mut e = Engine::new();
    ck(&mut e, "'(1,1),(2,2)'::lseg::text", "[(1,1),(2,2)]");
    ck(&mut e, "'[(1,1),(2,2)]'::lseg::text", "[(1,1),(2,2)]"); // bracketed still works
    ck(&mut e, "'(0,0),(3,4)'::lseg::text", "[(0,0),(3,4)]");
}

/// Geometric accessors: area/width/height/center (box), radius/diameter/area/
/// center (circle), length/isvertical/ishorizontal (lseg). PG18.4-verified.
#[test]
fn geometric_accessors() {
    let mut e = Engine::new();
    ck(&mut e, "area('(0,0),(2,3)'::box)::text", "6");
    ck(&mut e, "width('(0,0),(2,3)'::box)::text", "2");
    ck(&mut e, "height('(0,0),(2,3)'::box)::text", "3");
    ck(&mut e, "center('(0,0),(2,4)'::box)::text", "(1,2)");
    ck(&mut e, "radius('<(0,0),5>'::circle)::text", "5");
    ck(&mut e, "diameter('<(0,0),5>'::circle)::text", "10");
    ck(&mut e, "area('<(0,0),1>'::circle)::text", "3.141592653589793");
    ck(&mut e, "center('<(1,2),5>'::circle)::text", "(1,2)");
    ck(&mut e, "length('[(0,0),(3,4)]'::lseg)::text", "5");
    ck(&mut e, "isvertical('[(0,0),(0,4)]'::lseg)::text", "true");
    ck(&mut e, "ishorizontal('[(0,0),(3,0)]'::lseg)::text", "true");
}

/// npoints(path | polygon) — vertex count. PG18.4-verified.
#[test]
fn npoints_path_polygon() {
    let mut e = Engine::new();
    ck(&mut e, "npoints('[(0,0),(1,1),(2,2)]'::path)::text", "3");
    ck(&mut e, "npoints('((0,0),(1,1),(2,0),(3,1))'::polygon)::text", "4");
    ck(&mut e, "npoints('((0,0),(1,1),(2,0))'::polygon)::text", "3");
}

/// Geometric containment `@>` / `<@`: point in polygon (ray cast) / circle
/// (radius). PG18.4-verified.
#[test]
fn geo_containment() {
    let mut e = Engine::new();
    ck(&mut e, "('((0,0),(4,0),(4,4),(0,4))'::polygon @> point(2,2))::text", "true");
    ck(&mut e, "('((0,0),(4,0),(4,4),(0,4))'::polygon @> point(5,5))::text", "false");
    ck(&mut e, "(point(2,2) <@ '((0,0),(4,0),(4,4),(0,4))'::polygon)::text", "true");
    ck(&mut e, "('<(0,0),5>'::circle @> point(3,3))::text", "true");
    ck(&mut e, "('<(0,0),5>'::circle @> point(9,9))::text", "false");
    // control: array containment unaffected.
    ck(&mut e, "(ARRAY[1,2,3] @> ARRAY[2])::text", "true");
}

/// Geometric `TYPE 'literal'` typed-literal-prefix spelling (point '(1,2)'
/// == '(1,2)'::point). PG18.4-verified.
#[test]
fn geometric_typed_literals() {
    let mut e = Engine::new();
    ck(&mut e, "(point '(1,2)')::text", "(1,2)");
    ck(&mut e, "(circle '<(0,0),5>')::text", "<(0,0),5>");
    ck(&mut e, "(lseg '[(0,0),(1,1)]')::text", "[(0,0),(1,1)]");
    ck(&mut e, "(polygon '((0,0),(1,1),(2,0))')::text", "((0,0),(1,1),(2,0))");
    ck(&mut e, "(box '(1,1),(2,2)')::text", "(2,2),(1,1)");
    // it composes with operators now.
    ck(&mut e, "(circle '<(0,0),5>' @> point '(3,3)')::text", "true");
}

/// Range/multirange `TYPE 'literal'` typed-literal-prefix spelling. PG18.4-verified.
#[test]
fn range_typed_literals() {
    let mut e = Engine::new();
    ck(&mut e, "(int4range '[1,5)')::text", "[1,5)");
    ck(&mut e, "(int8range '[1,9)')::text", "[1,9)");
    ck(&mut e, "(numrange '[1.5,3.5)')::text", "[1.5,3.5)");
    ck(&mut e, "(daterange '[2024-01-01,2024-02-01)')::text", "[2024-01-01,2024-02-01)");
    // composes with operators.
    ck(&mut e, "(int4range '[1,5)' @> 3)::text", "true");
}

/// to_char(interval, fmt) — interval fields don't wrap (HH24 of 25h = 25),
/// months split into YYYY-MM, negatives keep their sign. PG18.4-verified.
#[test]
fn to_char_interval_fmt() {
    let mut e = Engine::new();
    ck(&mut e, "to_char(interval '1 day 2 hours', 'HH24:MI:SS')", "02:00:00");
    ck(&mut e, "to_char(interval '25 hours', 'HH24')", "25");
    ck(&mut e, "to_char(interval '1 day 2 hours', 'DD HH24')", "01 02");
    ck(&mut e, "to_char(interval '14 months', 'YYYY-MM')", "0001-02");
    ck(&mut e, "to_char(interval '1 day 2 hours 3 minutes 4 seconds', 'DD HH24:MI:SS')", "01 02:03:04");
    ck(&mut e, "to_char(interval '90 minutes', 'HH24:MI')", "01:30");
    ck(&mut e, "to_char(interval '2 hours', 'HH12 AM')", "02 AM");
    // negative time fields keep their sign (HH24/MI/SS/MM zero-pad after it).
    ck(&mut e, "to_char(interval '-2 hours', 'HH24')", "-02");
    // control: numeric/timestamp to_char unaffected.
    ck(&mut e, "to_char(timestamp '2024-03-15 14:30:45', 'HH24:MI:SS')", "14:30:45");
    ck(&mut e, "to_char(1234.5, 'FM9999.00')", "1234.50");
}

/// Geometric `&&` overlap for box and circle. PG18.4-verified.
#[test]
fn geo_overlap() {
    let mut e = Engine::new();
    ck(&mut e, "('(0,0),(2,2)'::box && '(1,1),(3,3)'::box)::text", "true");
    ck(&mut e, "('(0,0),(2,2)'::box && '(5,5),(6,6)'::box)::text", "false");
    ck(&mut e, "('<(0,0),5>'::circle && '<(3,0),5>'::circle)::text", "true");
    ck(&mut e, "('<(0,0),1>'::circle && '<(10,0),1>'::circle)::text", "false");
    // control: array overlap unaffected.
    ck(&mut e, "(ARRAY[1,2] && ARRAY[2,3])::text", "true");
}

/// Cross-type `<->` distance: point to lseg / box / circle. PG18.4-verified.
#[test]
fn point_geo_distance() {
    let mut e = Engine::new();
    ck(&mut e, "(point(0,0) <-> '(3,0),(4,0)'::lseg)::text", "3");
    ck(&mut e, "(point(0,4) <-> '(0,0),(3,0)'::lseg)::text", "4");
    ck(&mut e, "(point(2,2) <-> '(0,0),(4,0)'::lseg)::text", "2");
    ck(&mut e, "(point(5,5) <-> '(3,4),(5,6)'::box)::text", "0");
    ck(&mut e, "(point(0,0) <-> '(3,4),(5,6)'::box)::text", "5");
    ck(&mut e, "(point(10,0) <-> '<(0,0),5>'::circle)::text", "5");
    // both orders + point<->point control.
    ck(&mut e, "('(3,0),(4,0)'::lseg <-> point(0,0))::text", "3");
    ck(&mut e, "(point(0,0) <-> point(3,4))::text", "5");
}

/// sum(money) — the aggregate accumulator had no Money arm. PG18.4-verified.
/// (PG has no avg(money); SPG accepts it as a superset.)
#[test]
fn sum_money_agg() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE mt (m money)").unwrap();
    e.execute("INSERT INTO mt VALUES ('$10.00'),('$20.50'),('$5.25')").unwrap();
    let row = |e: &mut Engine, q: &str| -> String {
        match e.execute(q) {
            Ok(spg_engine::QueryResult::Rows { rows, .. }) if !rows.is_empty() => format!("{:?}", rows[0].values[0]),
            other => format!("{other:?}"),
        }
    };
    assert_eq!(row(&mut e, "SELECT sum(m)::text FROM mt"), "Text(\"$35.75\")");
    assert_eq!(row(&mut e, "SELECT max(m)::text FROM mt"), "Text(\"$20.50\")");
    // superset avg(money): (1000+2050+525)/3 = 1191.67c → $11.92 (rounded).
    assert_eq!(row(&mut e, "SELECT avg(m)::text FROM mt"), "Text(\"$11.92\")");
}

/// min/max over time / inet / bytea — value_cmp had no arms for these, so they
/// fell to `_ => Equal` and kept the first row. PG18.4-verified.
#[test]
fn maxmin_more_types() {
    let mut e = Engine::new();
    let row = |e: &mut Engine, setup: &[&str], q: &str| -> String {
        for s in setup { e.execute(s).unwrap(); }
        match e.execute(q) {
            Ok(spg_engine::QueryResult::Rows { rows, .. }) if !rows.is_empty() => format!("{:?}", rows[0].values[0]),
            other => format!("{other:?}"),
        }
    };
    assert_eq!(row(&mut e, &["CREATE TABLE tt (x time)","INSERT INTO tt VALUES ('10:00'),('14:00'),('02:30')"], "SELECT max(x)::text FROM tt"), "Text(\"14:00:00\")");
    assert_eq!(row(&mut e, &["CREATE TABLE tt2 (x time)","INSERT INTO tt2 VALUES ('10:00'),('14:00'),('02:30')"], "SELECT min(x)::text FROM tt2"), "Text(\"02:30:00\")");
    assert_eq!(row(&mut e, &["CREATE TABLE it (x inet)","INSERT INTO it VALUES ('192.168.1.1'),('10.0.0.1')"], "SELECT min(x)::text FROM it"), "Text(\"10.0.0.1\")");
    assert_eq!(row(&mut e, &["CREATE TABLE bt (x bytea)","INSERT INTO bt VALUES ('\\x01'),('\\xff'),('\\x80')"], "SELECT max(x)::text FROM bt"), "Text(\"\\\\xff\")");
}

/// ORDER BY over inet / bytea / uuid — the sort-key extractor rejected these
/// with "not supported"; they sort byte-wise in PG. PG18.4-verified.
#[test]
fn orderby_byte_types() {
    let mut e = Engine::new();
    let rows = |e: &mut Engine, setup: &[&str], q: &str| -> String {
        for s in setup { e.execute(s).unwrap(); }
        match e.execute(q) {
            Ok(spg_engine::QueryResult::Rows { rows, .. }) => rows.iter().map(|r| format!("{:?}", r.values[0])).collect::<Vec<_>>().join(","),
            other => format!("{other:?}"),
        }
    };
    assert_eq!(rows(&mut e, &["CREATE TABLE t2 (x inet)","INSERT INTO t2 VALUES ('192.168.1.1'),('10.0.0.1'),('172.16.0.1')"], "SELECT x::text FROM t2 ORDER BY x"), "Text(\"10.0.0.1\"),Text(\"172.16.0.1\"),Text(\"192.168.1.1\")");
    assert_eq!(rows(&mut e, &["CREATE TABLE t3 (x bytea)","INSERT INTO t3 VALUES ('\\xff'),('\\x01'),('\\x80')"], "SELECT x::text FROM t3 ORDER BY x"), "Text(\"\\\\x01\"),Text(\"\\\\x80\"),Text(\"\\\\xff\")");
    assert_eq!(rows(&mut e, &["CREATE TABLE t4 (x uuid)","INSERT INTO t4 VALUES ('550e8400-e29b-41d4-a716-446655440000'),('00000000-0000-0000-0000-000000000001')"], "SELECT x::text FROM t4 ORDER BY x"), "Text(\"00000000-0000-0000-0000-000000000001\"),Text(\"550e8400-e29b-41d4-a716-446655440000\")");
    assert_eq!(rows(&mut e, &["CREATE TABLE t5 (x bytea)","INSERT INTO t5 VALUES ('\\xff'),('\\x01'),('\\x80')"], "SELECT x::text FROM t5 ORDER BY x DESC"), "Text(\"\\\\xff\"),Text(\"\\\\x80\"),Text(\"\\\\x01\")");
}

/// ORDER BY interval — was explicitly rejected; PG orders by total time
/// (month = 30 days). PG18.4-verified: 1hr < 90min < 1day < 1mon.
#[test]
fn orderby_interval() {
    let mut e = Engine::new();
    let rows = |e: &mut Engine, setup: &[&str], q: &str| -> String {
        for s in setup { e.execute(s).unwrap(); }
        match e.execute(q) {
            Ok(spg_engine::QueryResult::Rows { rows, .. }) => rows.iter().map(|r| format!("{:?}", r.values[0])).collect::<Vec<_>>().join(","),
            other => format!("{other:?}"),
        }
    };
    assert_eq!(rows(&mut e, &["CREATE TABLE iv (x interval)","INSERT INTO iv VALUES ('1 day'),('1 hour'),('1 month'),('90 minutes')"], "SELECT x::text FROM iv ORDER BY x"),
        "Text(\"01:00:00\"),Text(\"01:30:00\"),Text(\"1 day\"),Text(\"1 mon\")");
    assert_eq!(rows(&mut e, &["CREATE TABLE iv2 (x interval)","INSERT INTO iv2 VALUES ('1 day'),('1 hour'),('1 month')"], "SELECT x::text FROM iv2 ORDER BY x DESC"),
        "Text(\"1 mon\"),Text(\"1 day\"),Text(\"01:00:00\")");
}

/// Comparison / BETWEEN / IN between a typed scalar and a string literal —
/// PG implicitly coerces the literal to the operand type. PG18.4-verified.
#[test]
fn cmp_text_coercion() {
    let mut e = Engine::new();
    ck(&mut e, "('12:00'::time BETWEEN '10:00' AND '14:00')::text", "true");
    ck(&mut e, "('\\x80'::bytea BETWEEN '\\x01' AND '\\xff')::text", "true");
    ck(&mut e, "('172.16.0.1'::inet BETWEEN '10.0.0.1' AND '192.168.1.1')::text", "true");
    ck(&mut e, "('10.0.0.1'::inet IN ('10.0.0.1','192.168.1.1'))::text", "true");
    ck(&mut e, "('1 day'::interval BETWEEN '1 hour' AND '1 month')::text", "true");
    ck(&mut e, "('14:00'::time > '10:00')::text", "true");
    ck(&mut e, "('10:00'::time = '10:00')::text", "true");
    // control: existing numeric-vs-text and text-vs-text unaffected.
    ck(&mut e, "('abc' < 'abd')::text", "true");
}

/// GREATEST/LEAST coerces a string literal to a sibling typed arg's type.
/// PG18.4-verified.
#[test]
fn greatest_least_coerce() {
    let mut e = Engine::new();
    ck(&mut e, "GREATEST('10:00'::time, '14:00')::text", "14:00:00");
    ck(&mut e, "LEAST('10:00'::time, '14:00')::text", "10:00:00");
    ck(&mut e, "GREATEST('\\x01'::bytea, '\\xff')::text", "\\xff");
    ck(&mut e, "GREATEST('2024-01-01'::date, '2024-06-01')::text", "2024-06-01");
    // control: all-text greatest unaffected.
    ck(&mut e, "GREATEST('abc', 'abd', 'aaa')::text", "abd");
    ck(&mut e, "GREATEST(3, 7, 5)::text", "7");
}

/// `<date|timestamp> - <string literal>` coerces the literal to the operand
/// type (date-date → int days, ts-ts → interval). PG18.4-verified.
#[test]
fn temporal_minus_text() {
    let mut e = Engine::new();
    ck(&mut e, "('2024-01-15'::date - '2024-01-10')::text", "5");
    ck(&mut e, "('2024-06-01'::timestamp - '2024-05-01')::text", "31 days");
    ck(&mut e, "('14:00'::time - '10:00')::text", "04:00:00");
    // control: date-date, date-int still work.
    ck(&mut e, "('2024-01-15'::date - '2024-01-10'::date)::text", "5");
    ck(&mut e, "('2024-01-15'::date - 5)::text", "2024-01-10");
}

/// v7.37 C.5 (A.2a) — the engine already supports concurrent, isolated
/// transactions addressed by distinct `TxId`s: the `tx_catalogs` COW-shadow
/// model gives each open tx its own snapshot. This proves the A.1 hypothesis
/// (the data model is ready; only the server's per-connection tx_id threading
/// is missing) and anchors the A.2 server wiring against regression.
#[test]
fn concurrent_tx_isolation() {
    use spg_engine::IMPLICIT_TX;
    let mut e = Engine::new();
    e.execute_in("CREATE TABLE t (x int)", IMPLICIT_TX).unwrap();
    let tx1 = e.alloc_tx_id();
    let tx2 = e.alloc_tx_id();
    e.execute_in("BEGIN", tx1).unwrap();
    e.execute_in("BEGIN", tx2).unwrap();
    e.execute_in("INSERT INTO t VALUES (1)", tx1).unwrap();
    let cnt = |e: &mut Engine, tx| -> String {
        match e.execute_in("SELECT count(*) FROM t", tx) {
            Ok(spg_engine::QueryResult::Rows { rows, .. }) if !rows.is_empty() => format!("{:?}", rows[0].values[0]),
            other => format!("{other:?}"),
        }
    };
    assert_eq!(cnt(&mut e, tx1), "BigInt(1)", "tx1 sees its own uncommitted insert");
    assert_eq!(cnt(&mut e, tx2), "BigInt(0)", "tx2 is isolated from tx1's uncommitted insert");
    assert!(e.is_tx_open(tx1) && e.is_tx_open(tx2), "both tx open");
    e.execute_in("COMMIT", tx1).unwrap();
    assert_eq!(cnt(&mut e, tx2), "BigInt(0)", "tx2 keeps its snapshot after tx1 commits (SI)");
    assert_eq!(cnt(&mut e, IMPLICIT_TX), "BigInt(1)", "autocommit reads main catalog with tx1's committed row");
    assert!(!e.is_tx_open(tx1), "tx1 closed after commit");
    e.execute_in("ROLLBACK", tx2).unwrap();
    assert!(!e.is_tx_open(tx2), "tx2 closed after rollback");
}

/// v7.37 C.5 (A.2b groundwork) — proves the "Design A" server-wiring plan:
/// a connection may pass its own stable `tx_id` for EVERY statement. An
/// autocommit statement (no BEGIN) under a per-connection tx_id behaves exactly
/// like IMPLICIT_TX — it writes the main catalog, is visible to all other
/// connections, and leaves no open shadow. This lets the server give each
/// connection a unique tx_id at accept without a BEGIN/COMMIT detection step.
#[test]
fn autocommit_under_conn_tx() {
    use spg_engine::IMPLICIT_TX;
    let mut e = Engine::new();
    let conn_a = e.alloc_tx_id();
    let conn_b = e.alloc_tx_id();
    e.execute_in("CREATE TABLE t (x int)", conn_a).unwrap();
    e.execute_in("INSERT INTO t VALUES (1)", conn_a).unwrap();
    let cnt = |e: &mut Engine, tx| -> String {
        match e.execute_in("SELECT count(*) FROM t", tx) {
            Ok(spg_engine::QueryResult::Rows { rows, .. }) if !rows.is_empty() => format!("{:?}", rows[0].values[0]),
            other => format!("{other:?}"),
        }
    };
    assert_eq!(cnt(&mut e, conn_b), "BigInt(1)", "conn_b sees conn_a's autocommit write");
    assert_eq!(cnt(&mut e, IMPLICIT_TX), "BigInt(1)", "IMPLICIT_TX sees it too");
    assert!(!e.is_tx_open(conn_a), "autocommit leaves no open shadow under conn_a");
}

/// v7.37 D.1 — COALESCE result-type coercion: a typed sibling branch coerces
/// an untyped string-literal branch to that type. PG18.4-verified.
#[test]
fn coalesce_result_type() {
    let mut e = Engine::new();
    ck(&mut e, "COALESCE(NULL::time, '12:00')::text", "12:00:00");
    ck(&mut e, "COALESCE(NULL::date, '2024-06-01')::text", "2024-06-01");
    ck(&mut e, "COALESCE(NULL::timestamp, '2024-06-01 09:30')::text", "2024-06-01 09:30:00");
    ck(&mut e, "COALESCE('14:30'::time, '09:00')::text", "14:30:00");
    // control: all-text COALESCE returns the raw text unchanged.
    ck(&mut e, "COALESCE(NULL, 'hello')::text", "hello");
    ck(&mut e, "COALESCE(NULL::int, 42)::text", "42");
}

/// v7.37 D.1 — CASE result-type coercion (same rule as COALESCE). PG18.4-verified.
#[test]
fn case_result_type() {
    let mut e = Engine::new();
    ck(&mut e, "(CASE WHEN false THEN '10:00'::time ELSE '11:30' END)::text", "11:30:00");
    ck(&mut e, "(CASE WHEN true THEN '10:00'::time ELSE '11:30' END)::text", "10:00:00");
    ck(&mut e, "(CASE WHEN false THEN '2024-01-01'::date ELSE '2024-06-01' END)::text", "2024-06-01");
    // control: all-text CASE unchanged.
    ck(&mut e, "(CASE WHEN true THEN 'a' ELSE 'b' END)::text", "a");
}

/// v7.37 D.2 — `<time|interval|timestamp> + <string literal>` coerces the
/// literal to INTERVAL (the only `+` those types have). PG18.4-verified.
/// `date + text` stays a "not unique" error, matching PG.
#[test]
fn temporal_plus_text() {
    let mut e = Engine::new();
    ck(&mut e, "('10:00'::time + '30 minutes')::text", "10:30:00");
    ck(&mut e, "('1 day'::interval + '12 hours')::text", "1 day 12:00:00");
    ck(&mut e, "('30 minutes' + '10:00'::time)::text", "10:30:00");
    ck(&mut e, "('2024-01-01 10:00'::timestamp + '1 hour')::text", "2024-01-01 11:00:00");
    // control: time + interval (typed) still works.
    ck(&mut e, "('10:00'::time + '30 minutes'::interval)::text", "10:30:00");
    // date + text stays an error (ambiguous) — matches PG.
    assert_eq!(cast(&mut e, "'2024-01-01'::date + '5 days'"), "ERR");
}



/// v7.37 D.5 — polygon text accepts the bare (no outer-paren) point list, like
/// PG: both `(0,0),(1,1)` and `((0,0),(1,1))`. Unblocks npoints(polygon) and
/// the `#` operator's functional equivalent. PG18.4-verified.
#[test]
fn polygon_bare_input() {
    let mut e = Engine::new();
    ck(&mut e, "('(0,0),(4,0),(4,3),(0,3)'::polygon)::text", "((0,0),(4,0),(4,3),(0,3))");
    ck(&mut e, "('((0,0),(4,0),(4,3),(0,3))'::polygon)::text", "((0,0),(4,0),(4,3),(0,3))");
    ck(&mut e, "npoints('(0,0),(4,0),(4,3),(0,3)'::polygon)::text", "4");
    ck(&mut e, "npoints('((0,0),(1,1))'::polygon)::text", "2");
}

/// v7.37 D.5 — path text accepts the bare (no-bracket) point list as a closed
/// path, like PG; `[...]` stays open, `(...)` stays closed. PG18.4-verified.
#[test]
fn path_bare_input() {
    let mut e = Engine::new();
    ck(&mut e, "('[(0,0),(1,1),(2,0)]'::path)::text", "[(0,0),(1,1),(2,0)]");
    ck(&mut e, "('((0,0),(1,1),(2,0))'::path)::text", "((0,0),(1,1),(2,0))");
    ck(&mut e, "('(0,0),(1,1),(2,0)'::path)::text", "((0,0),(1,1),(2,0))");
    ck(&mut e, "npoints('(0,0),(1,1),(2,0)'::path)::text", "3");
}

/// v7.37 D.5 — circle and box accept the bare (unwrapped) numeric input like PG:
/// circle `x,y,r`, box `x1,y1,x2,y2`. PG18.4-verified.
#[test]
fn circle_box_bare_input() {
    let mut e = Engine::new();
    ck(&mut e, "('1,1,5'::circle)::text", "<(1,1),5>");
    ck(&mut e, "('((1,1),5)'::circle)::text", "<(1,1),5>");
    ck(&mut e, "('0,0,2,2'::box)::text", "(2,2),(0,0)");
    ck(&mut e, "('(0,0),(2,2)'::box)::text", "(2,2),(0,0)");
}

/// v7.37 D — INTERVAL accepts ISO 8601 durations (`P1Y2M3DT4H`) and PG's
/// year-month shorthand (`1-2`). PG18.4-verified.
#[test]
fn interval_iso8601_and_yearmonth() {
    let mut e = Engine::new();
    ck(&mut e, "('P1Y2M3DT4H'::interval)::text", "1 year 2 mons 3 days 04:00:00");
    ck(&mut e, "('P1Y2M3D'::interval)::text", "1 year 2 mons 3 days");
    ck(&mut e, "('PT1H30M'::interval)::text", "01:30:00");
    ck(&mut e, "('1-2'::interval)::text", "1 year 2 mons");
    // control: the `<n> <unit>` word form still works.
    ck(&mut e, "('1 year 2 mons 3 days'::interval)::text", "1 year 2 mons 3 days");
}


/// v7.37 D — integer text accepts PG 16+ radix prefixes (0x/0o/0b) and `_`
/// digit separators, both via `::int` cast and the generic coerce path.
/// PG18.4-verified.
#[test]
fn integer_radix_and_underscores() {
    let mut e = Engine::new();
    ck(&mut e, "('0x1F'::int4)::text", "31");
    ck(&mut e, "('1_000'::int4)::text", "1000");
    ck(&mut e, "('0o17'::int4)::text", "15");
    ck(&mut e, "('0b101'::int4)::text", "5");
    ck(&mut e, "('-0x10'::bigint)::text", "-16");
    ck(&mut e, "('1_000_000'::bigint)::text", "1000000");
    // control: plain decimal unchanged.
    ck(&mut e, "('42'::int4)::text", "42");
}


/// v7.37 D — numeric text accepts scientific notation (`1e3`, `1.5e2`, `3E-4`),
/// like PG; the exponent folds into the decimal scale. PG18.4-verified.
#[test]
fn numeric_scientific_notation() {
    let mut e = Engine::new();
    ck(&mut e, "('1e3'::numeric)::text", "1000");
    ck(&mut e, "('1.5e2'::numeric)::text", "150");
    ck(&mut e, "('1.5e-2'::numeric)::text", "0.015");
    ck(&mut e, "('-2.5E3'::numeric)::text", "-2500");
    // control: plain decimal unchanged.
    ck(&mut e, "('3.14'::numeric)::text", "3.14");
}


/// v7.37 D — DATE accepts the compact ISO basic form `YYYYMMDD` and both DATE
/// and TIMESTAMP accept the special value `epoch`. PG18.4-verified.
#[test]
fn date_compact_and_epoch() {
    let mut e = Engine::new();
    ck(&mut e, "('20240115'::date)::text", "2024-01-15");
    ck(&mut e, "('epoch'::date)::text", "1970-01-01");
    ck(&mut e, "('epoch'::timestamp)::text", "1970-01-01 00:00:00");
    // control: hyphenated date unchanged; invalid compact rejected downstream.
    ck(&mut e, "('2024-01-15'::date)::text", "2024-01-15");
}


/// v7.37 D — TIME accepts the `allballs` special value (midnight) and the
/// `24:00:00` end-of-day sentinel; `24:00:01` stays rejected. PG18.4-verified.
#[test]
fn time_allballs_and_24h() {
    let mut e = Engine::new();
    ck(&mut e, "('allballs'::time)::text", "00:00:00");
    ck(&mut e, "('24:00:00'::time)::text", "24:00:00");
    // control: normal time + rejection of out-of-range.
    ck(&mut e, "('12:30'::time)::text", "12:30:00");
    assert_eq!(cast(&mut e, "'24:00:01'::time"), "ERR");
}





/// v7.37 D — round(numeric, scale) does exact mantissa rounding instead of
/// routing through f64, so `round(1.255::numeric, 2)` = 1.26 like PG (1.255 has
/// no exact f64 and would otherwise land at 1.25). PG18.4-verified.
#[test]
fn round_numeric_exact_scale() {
    let mut e = Engine::new();
    ck(&mut e, "round(1.255::numeric, 2)::text", "1.26");
    ck(&mut e, "round(2.675::numeric, 2)::text", "2.68");
    ck(&mut e, "round((-1.255)::numeric, 2)::text", "-1.26");
    ck(&mut e, "round(1.5::numeric, 3)::text", "1.500");
    // control: 1-arg numeric round unchanged.
    ck(&mut e, "round(2.5::numeric)::text", "3");
}


/// v7.37 D — unary minus works on NUMERIC / SMALLINT / MONEY (only Int / BigInt
/// / Float / Interval were handled, so `-1.255::numeric` errored). PG18.4-verified.
#[test]
fn unary_minus_numeric_smallint() {
    let mut e = Engine::new();
    ck(&mut e, "(-1.255::numeric)::text", "-1.255");
    ck(&mut e, "(-(3::int2))::text", "-3");
    ck(&mut e, "round(-1.255::numeric, 2)::text", "-1.26");
    // control: existing int / float unary minus unchanged.
    ck(&mut e, "(-5)::text", "-5");
}




/// v7.37 D — to_char(n, 'RN') renders Roman numerals (1..=3999), 15-char
/// right-justified; `FM` trims; out-of-range → 15 `#`. PG18.4-verified.
#[test]
fn to_char_roman_numerals() {
    let mut e = Engine::new();
    ck(&mut e, "to_char(42, 'FMRN')", "XLII");
    ck(&mut e, "to_char(3999, 'FMRN')", "MMMCMXCIX");
    ck(&mut e, "to_char(1, 'FMRN')", "I");
    ck(&mut e, "to_char(42, 'RN')", "           XLII");
    ck(&mut e, "to_char(4000, 'FMRN')", "###############");
    ck(&mut e, "to_char(0, 'FMRN')", "###############");
}

/// v7.37 D — format() honors the `%[n$][-][width]s` field width (right-justify,
/// `-` left-justify; never truncates). PG18.4-verified.
#[test]
fn format_width_spec() {
    let mut e = Engine::new();
    ck(&mut e, "format('[%3s]', 'x')", "[  x]");
    ck(&mut e, "format('[%-3s]', 'x')", "[x  ]");
    ck(&mut e, "format('[%1$3s]', 'y')", "[  y]");
    ck(&mut e, "format('[%3s]', 'abcd')", "[abcd]");
    // control: no-width forms unchanged.
    ck(&mut e, "format('%1$s %1$s', 'x')", "x x");
    ck(&mut e, "format('%I', 'my table')", "\"my table\"");
}

/// v7.37 D.9 (slice 1) — non-capturing groups `(?:...)` parse correctly (the
/// `?:` marker was previously parsed as regex atoms, so the group never
/// matched). PG18.4-verified.
#[test]
fn regex_noncapturing_group() {
    let mut e = Engine::new();
    ck(&mut e, "('abcabc' ~ '(?:abc)+')::text", "true");
    ck(&mut e, "regexp_replace('xayaz', '(?:a)', 'Q', 'g')", "xQyQz");
    ck(&mut e, "('foobar' ~ '(?:foo)(?:bar)')::text", "true");
    // control: a plain capturing group still matches as before.
    ck(&mut e, "('abcabc' ~ '(abc)+')::text", "true");
}

/// v7.37 D.9 (slice 1b) — leading inline option group `(?i)` applies
/// case-insensitive matching to the whole pattern (other flags accepted +
/// ignored). PG18.4-verified. Verifies `(?:...)` is NOT mistaken for a flag group.
#[test]
fn regex_inline_flags() {
    let mut e = Engine::new();
    ck(&mut e, "('ABC' ~ '(?i)abc')::text", "true");
    ck(&mut e, "('abc' ~ '(?i)ABC')::text", "true");
    ck(&mut e, "regexp_replace('AbCd', '(?i)[a-z]', 'Q', 'g')", "QQQQ");
    // control: case-sensitive without the flag; `(?:...)` still non-capturing.
    ck(&mut e, "('ABC' ~ 'abc')::text", "false");
    ck(&mut e, "('abcabc' ~ '(?:abc)+')::text", "true");
}

/// v7.37 D — regex numeric + character-entry escapes: `\xHH`/`\uHHHH` and
/// `\t \n \r \f \v \a \e`. Previously these fell through to a literal letter.
/// PG18.4-verified.
#[test]
fn regex_char_escapes() {
    let mut e = Engine::new();
    ck(&mut e, "('A' ~ '^\\x41$')::text", "true");
    ck(&mut e, "('AB' ~ '^\\x41\\x42$')::text", "true");
    ck(&mut e, "(E'\\t' ~ '^\\t$')::text", "true");
    ck(&mut e, "(E'\\n' ~ '^\\n$')::text", "true");
    ck(&mut e, "('A' ~ '^\\u0041$')::text", "true");
    // control: an unknown escape is still the literal char.
    ck(&mut e, "('q' ~ '^\\q$')::text", "true");
}

/// v7.37 D.9 (slice 2) — zero-width lookahead assertions `(?=...)` / `(?!...)`.
/// PG18.4-verified.
#[test]
fn regex_lookahead() {
    let mut e = Engine::new();
    ck(&mut e, "('abc' ~ 'a(?=b)')::text", "true");
    ck(&mut e, "('axc' ~ 'a(?=b)')::text", "false");
    ck(&mut e, "('abc' ~ 'a(?!x)')::text", "true");
    ck(&mut e, "('axc' ~ 'a(?!x)')::text", "false");
    // zero-width: the lookahead consumes nothing, so 'b' still must match after.
    ck(&mut e, "('abc' ~ '^a(?=b)bc$')::text", "true");
    // control: (?:...) / (?i) still work alongside.
    ck(&mut e, "('abcabc' ~ '(?:abc)+')::text", "true");
    ck(&mut e, "('ABC' ~ '(?i)a(?=b)')::text", "true");
}

/// v7.37 D — inet \u00b1 integer shifts the address (keeping family + prefix), and
/// integer + inet is commutative. Verified via host()/masklen (display-agnostic
/// so the /32-elision rendering nuance doesn't affect it). PG18.4-verified.
#[test]
fn inet_int_arithmetic() {
    let mut e = Engine::new();
    ck(&mut e, "host('192.168.1.5'::inet + 10)", "192.168.1.15");
    ck(&mut e, "host('192.168.1.20'::inet - 5)", "192.168.1.15");
    ck(&mut e, "host(10 + '192.168.1.5'::inet)", "192.168.1.15");
    ck(&mut e, "host('192.168.1.5/24'::inet + 1)", "192.168.1.6");
    ck(&mut e, "masklen('192.168.1.5/24'::inet + 1)::text", "24");
    // control: inet - inet still yields the address count.
    ck(&mut e, "('192.168.1.20'::inet - '192.168.1.5'::inet)::text", "15");
}

/// v7.37 D — box accepts the fully-wrapped `((x1,y1),(x2,y2))` form (only the
/// bare and single-paren forms parsed before), so `box @> point` /
/// `point <@ box` on such literals now work. PG18.4-verified.
#[test]
fn box_double_paren_input() {
    let mut e = Engine::new();
    ck(&mut e, "('((0,0),(2,2))'::box)::text", "(2,2),(0,0)");
    ck(&mut e, "('((0,0),(2,2))'::box @> '(1,1)'::point)::text", "true");
    ck(&mut e, "('(1,1)'::point <@ '((0,0),(2,2))'::box)::text", "true");
    ck(&mut e, "(area('((0,0),(2,2))'::box))::text", "4");
    // control: the bare + single-paren forms still parse.
    ck(&mut e, "('(0,0),(2,2)'::box)::text", "(2,2),(0,0)");
    ck(&mut e, "('0,0,2,2'::box)::text", "(2,2),(0,0)");
}

/// v7.37 D — macaddr bitwise operators `&` / `|` (byte-wise) and unary `~`.
/// PG18.4-verified.
#[test]
fn macaddr_bitwise() {
    let mut e = Engine::new();
    ck(&mut e, "('08:00:2b:01:02:03'::macaddr & '00:00:00:ff:ff:ff'::macaddr)::text", "00:00:00:01:02:03");
    ck(&mut e, "('08:00:2b:01:02:03'::macaddr | '00:00:00:ff:ff:ff'::macaddr)::text", "08:00:2b:ff:ff:ff");
    ck(&mut e, "(~ '08:00:2b:01:02:03'::macaddr)::text", "f7:ff:d4:fe:fd:fc");
    // control: macaddr comparison still works.
    ck(&mut e, "('08:00:2b:01:02:03'::macaddr = '08:00:2b:01:02:03'::macaddr)::text", "true");
}

/// v7.37 D — macaddr8 (EUI-64) bitwise operators `&` / `|` and unary `~`,
/// mirroring the 6-byte macaddr support. PG18.4-verified.
#[test]
fn macaddr8_bitwise() {
    let mut e = Engine::new();
    ck(&mut e, "('08:00:2b:01:02:03:04:05'::macaddr8 & 'ff:ff:ff:00:00:00:ff:ff'::macaddr8)::text", "08:00:2b:00:00:00:04:05");
    ck(&mut e, "('08:00:2b:01:02:03:04:05'::macaddr8 | '00:00:00:ff:ff:ff:00:00'::macaddr8)::text", "08:00:2b:ff:ff:ff:04:05");
    ck(&mut e, "(~ '08:00:2b:01:02:03:04:05'::macaddr8)::text", "f7:ff:d4:fe:fd:fc:fb:fa");
    // control: macaddr8 comparison still works.
    ck(&mut e, "('08:00:2b:01:02:03:04:05'::macaddr8 = '08:00:2b:01:02:03:04:05'::macaddr8)::text", "true");
}

/// v7.37 D — lseg accepts the fully-wrapped `((x1,y1),(x2,y2))` form (only
/// bracketed + bare parsed before), matching the box fix. PG18.4-verified
/// (lseg renders in [ ] canonical form; point <-> lseg distance works).
#[test]
fn lseg_double_paren_input() {
    let mut e = Engine::new();
    ck(&mut e, "('((0,0),(4,0))'::lseg)::text", "[(0,0),(4,0)]");
    ck(&mut e, "('(1,1)'::point <-> '((0,0),(4,0))'::lseg)::text", "1");
    // control: the bracketed + bare forms still parse.
    ck(&mut e, "('[(0,0),(4,0)]'::lseg)::text", "[(0,0),(4,0)]");
    ck(&mut e, "('(0,0),(4,0)'::lseg)::text", "[(0,0),(4,0)]");
}

/// v7.37 D — point <-> line distance and box @> / <@ box containment
/// (extending point-to-{lseg,box,circle} distance and box @> point
/// containment). PG18.4-verified.
#[test]
fn geo_line_distance_and_box_containment() {
    let mut e = Engine::new();
    // point-to-line distance: point (0,0), line x - 5 = 0 → distance 5.
    ck(&mut e, "(('(0,0)'::point) <-> ('{1,0,-5}'::line))::text", "5");
    ck(&mut e, "('((0,0),(2,2))'::box @> '((0.5,0.5),(1,1))'::box)::text", "true");
    ck(&mut e, "('((0.5,0.5),(1,1))'::box <@ '((0,0),(2,2))'::box)::text", "true");
    ck(&mut e, "('((0,0),(2,2))'::box @> '((1,1),(3,3))'::box)::text", "false");
    // control: box @> point still works.
    ck(&mut e, "('((0,0),(2,2))'::box @> '(1,1)'::point)::text", "true");
}

/// v7.37 D — box <-> box distance is the distance between box centres (PG18.4:
/// overlapping boxes still return the centre distance, not 0). Closes the
/// geometric-distance operator family. PG18.4-verified.
#[test]
fn box_box_distance() {
    let mut e = Engine::new();
    ck(&mut e, "('((0,0),(2,2))'::box <-> '((10,0),(12,2))'::box)::text", "10");
    ck(&mut e, "('((0,0),(2,2))'::box <-> '((0,10),(2,12))'::box)::text", "10");
    // overlapping boxes: centre distance sqrt(2), not 0.
    ck(&mut e, "('((0,0),(2,2))'::box <-> '((1,1),(3,3))'::box)::text", "1.4142135623730951");
    // control: point <-> point distance still works.
    ck(&mut e, "('(0,0)'::point <-> '(3,4)'::point)::text", "5");
}

/// v7.37 D — get_bit / set_bit accept bit/varbit operands (MSB-first, bit 0
/// leftmost), not just bytea. PG18.4-verified.
#[test]
fn bit_get_set_bit() {
    let mut e = Engine::new();
    ck(&mut e, "get_bit(B'10110', 1)::text", "0");
    ck(&mut e, "get_bit(B'10110', 0)::text", "1");
    ck(&mut e, "get_bit(B'10110', 4)::text", "0");
    ck(&mut e, "(set_bit(B'10110', 1, 1))::text", "11110");
    ck(&mut e, "(set_bit(B'10110', 0, 0))::text", "00110");
    ck(&mut e, "(set_bit(B'10110', 0, 1))::text", "10110");
    // control: get_bit/set_bit on bytea still work.
    ck(&mut e, "get_bit('\\x00'::bytea, 0)::text", "0");
}

/// v7.37 D — length(tsvector) / tsvector_length(tsvector) return the lexeme
/// count (both previously errored on the TsVector value). PG18.4-verified.
#[test]
fn tsvector_length_ops() {
    let mut e = Engine::new();
    ck(&mut e, "length('a:1 b:2 c:3'::tsvector)::text", "3");
    ck(&mut e, "tsvector_length('a:1 b:2 c:3'::tsvector)::text", "3");
    ck(&mut e, "length('the quick brown fox'::tsvector)::text", "4");
    // control: length(text) unchanged.
    ck(&mut e, "length('abcd')::text", "4");
}

/// v7.37 D.12 — position(bit in bit) → strpos does a bit-level MSB-first search
/// (was byte-comparing the MSB-packed forms → wrong 0). PG18.4-verified.
#[test]
fn bit_position() {
    let mut e = Engine::new();
    ck(&mut e, "position(B'11' in B'0110')::text", "2");
    ck(&mut e, "position(B'01' in B'0110')::text", "1");
    ck(&mut e, "position(B'10' in B'0110')::text", "3");
    ck(&mut e, "position(B'111' in B'0110')::text", "0");
    ck(&mut e, "position(B'0' in B'0110')::text", "1");
    // control: text + bytea position still work.
    ck(&mut e, "position('cd' in 'abcd')::text", "3");
}

/// v7.37 D — substring(bit FROM s FOR l) does a bit-level slice (MSB-first,
/// 1-based), repacked into a bit string. Previously errored. PG18.4-verified.
#[test]
fn bit_substring() {
    let mut e = Engine::new();
    ck(&mut e, "substring(B'10110' from 2 for 3)::text", "011");
    ck(&mut e, "substring(B'10110' from 3)::text", "110");
    ck(&mut e, "substring(B'10110' for 2)::text", "10");
    ck(&mut e, "substring(B'10110' from 2 for 10)::text", "0110");
    // control: text substring (FROM/FOR + FOR-only) works too.
    ck(&mut e, "substring('abcdef' from 2 for 3)::text", "bcd");
    ck(&mut e, "substring('abcdef' for 3)::text", "abc");
}

/// v7.37 D — length(path) (sum of segment lengths, closed adds wrap-around) and
/// path + path concatenation. Both previously errored. PG18.4-verified.
#[test]
fn path_length_and_concat() {
    let mut e = Engine::new();
    ck(&mut e, "(length('[(0,0),(3,4)]'::path))::text", "5");
    ck(&mut e, "(length('[(0,0),(3,0),(3,4)]'::path))::text", "7");
    // closed path adds the wrap-around segment (3,4)->(0,0) = 5 → 7 + 5 = 12.
    ck(&mut e, "(length('((0,0),(3,0),(3,4))'::path))::text", "12");
    ck(&mut e, "('[(0,0),(3,0)]'::path + '[(3,0),(3,4)]'::path)::text", "[(0,0),(3,0),(3,0),(3,4)]");
    // control: npoints still works.
    ck(&mut e, "(npoints('[(0,0),(3,4),(5,5)]'::path))::text", "3");
}

/// v7.37 D — circle <-> circle distance is the boundary gap (centre distance
/// minus both radii, clamped to 0 on overlap). Previously errored. PG18.4-verified.
#[test]
fn circle_circle_distance() {
    let mut e = Engine::new();
    ck(&mut e, "('<(0,0),2>'::circle <-> '<(10,0),2>'::circle)::text", "6");
    // overlapping circles: gap clamped to 0.
    ck(&mut e, "('<(0,0),3>'::circle <-> '<(4,0),3>'::circle)::text", "0");
    // control: point <-> circle still works.
    ck(&mut e, "('(0,0)'::point <-> '<(5,0),1>'::circle)::text", "4");
}

/// v7.37 D — cidr ± integer behaves as PG's implicit cidr->inet cast then
/// shifts the address. Previously errored. PG18.4-verified.
#[test]
fn cidr_int_arithmetic() {
    let mut e = Engine::new();
    ck(&mut e, "host('192.168.1.0/24'::cidr + 5)", "192.168.1.5");
    ck(&mut e, "masklen('192.168.1.0/24'::cidr + 5)::text", "24");
    ck(&mut e, "host('192.168.2.0/24'::cidr - 256)", "192.168.1.0");
    ck(&mut e, "host('10.0.0.0/8'::cidr + 300)", "10.0.1.44");
    // control: inet + int still works.
    ck(&mut e, "host('192.168.1.5'::inet + 10)", "192.168.1.15");
}

/// v7.37 D — numeric % numeric rescales to the shared scale then takes the
/// truncated remainder. Previously errored. PG18.4-verified.
#[test]
fn numeric_modulo() {
    let mut e = Engine::new();
    ck(&mut e, "(12.5::numeric % 5)::text", "2.5");
    ck(&mut e, "(10.75::numeric % 2.5)::text", "0.75");
    ck(&mut e, "((-12.5)::numeric % 5)::text", "-2.5");
    ck(&mut e, "(100::numeric % 7)::text", "2");
    // control: integer modulo unchanged.
    ck(&mut e, "(17 % 5)::text", "2");
}
