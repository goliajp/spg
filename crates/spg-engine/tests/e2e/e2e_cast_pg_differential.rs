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

/// v7.37 D.13 — window functions over a derived table (subquery / VALUES).
/// Previously threw TableNotFound because the window path only looked the
/// source up in the catalog by name. PG18.4-verified.
#[test]
fn window_over_derived_table() {
    let mut e = Engine::new();
    let q = |e: &mut Engine, sql: &str| -> String {
        match e.execute(sql) {
            Ok(QueryResult::Rows { rows, .. }) => {
                let c: Vec<String> = rows.iter().map(|r| match &r.values[0] {
                    Value::Null => "N".into(), Value::Text(s) => s.to_string(), o => format!("{o:?}"),
                }).collect();
                format!("[{}]", c.join(","))
            }
            Ok(o) => format!("<NON:{o:?}>"), Err(e2) => format!("ERR:{e2:?}"),
        }
    };
    assert_eq!(q(&mut e, "SELECT (row_number() OVER (ORDER BY v))::text FROM (VALUES (10),(20),(30)) t(v)"), "[1,2,3]");
    assert_eq!(q(&mut e, "SELECT (nth_value(v,2) OVER (ORDER BY v))::text FROM (VALUES (10),(20),(30)) t(v)"), "[N,20,20]");
    assert_eq!(q(&mut e, "SELECT (lag(v,1,0) OVER (ORDER BY v))::text FROM (SELECT v FROM (VALUES (5),(10),(15)) x(v)) s(v)"), "[0,5,10]");
    assert_eq!(q(&mut e, "SELECT (rank() OVER (ORDER BY v))::text FROM (VALUES (1),(1),(3)) t(v)"), "[1,1,3]");
}

/// v7.37 D — XMLELEMENT(NAME ident [, content …]) — parse the NAME-keyword
/// syntax and build the element (self-closing when empty; xml content verbatim,
/// text content escaped). PG18.4-verified.
#[test]
fn xmlelement_basic() {
    let mut e = Engine::new();
    ck(&mut e, "(xmlelement(name foo))::text", "<foo/>");
    ck(&mut e, "(xmlelement(name foo, 'bar'))::text", "<foo>bar</foo>");
    ck(&mut e, "(xmlelement(name foo, 'a', 'b', 'c'))::text", "<foo>abc</foo>");
    ck(&mut e, "(xmlelement(name \"Foo-Bar\", 42))::text", "<Foo-Bar>42</Foo-Bar>");
    ck(&mut e, "(xmlelement(name item, xmlelement(name sub, 'x')))::text", "<item><sub>x</sub></item>");
    ck(&mut e, "(xmlelement(name t, '<b>'))::text", "<t>&lt;b&gt;</t>");
}

/// v7.37 D — XMLFOREST(value AS name, …) builds a concatenation of named
/// elements. PG18.4-verified.
#[test]
fn xmlforest_basic() {
    let mut e = Engine::new();
    ck(&mut e, "(xmlforest('a' AS foo, 'b' AS bar))::text", "<foo>a</foo><bar>b</bar>");
    ck(&mut e, "(xmlforest(42 AS num))::text", "<num>42</num>");
    ck(&mut e, "(xmlforest('x' AS \"My-Col\"))::text", "<My-Col>x</My-Col>");
    ck(&mut e, "(xmlforest('<b>' AS t))::text", "<t>&lt;b&gt;</t>");
    // nested xmlelement content stays verbatim inside the forest element.
    ck(&mut e, "(xmlforest(xmlelement(name s, 'x') AS wrap))::text", "<wrap><s>x</s></wrap>");
}

/// v7.37 D — tsquery @> / <@ tsquery containment by lexeme set. PG18.4-verified.
#[test]
fn tsquery_containment() {
    let mut e = Engine::new();
    ck(&mut e, "('cat & dog'::tsquery @> 'cat'::tsquery)::text", "true");
    ck(&mut e, "('cat'::tsquery @> 'cat & dog'::tsquery)::text", "false");
    ck(&mut e, "('cat & dog'::tsquery @> 'bird'::tsquery)::text", "false");
    ck(&mut e, "('cat'::tsquery <@ 'cat & dog'::tsquery)::text", "true");
    ck(&mut e, "('cat | dog'::tsquery @> 'dog'::tsquery)::text", "true");
}

/// v7.37 D — range @> numeric element (numeric operand no longer stolen by the
/// numeric fast-path before the range containment arm). PG18.4-verified.
#[test]
fn numrange_contains_numeric() {
    let mut e = Engine::new();
    ck(&mut e, "(numrange(1.5, 3.5) @> 2.0::numeric)::text", "true");
    ck(&mut e, "(numrange(1.5, 3.5) @> 4.0::numeric)::text", "false");
    ck(&mut e, "('[1.5,3.5)'::numrange @> 2.0::numeric)::text", "true");
    ck(&mut e, "(2.0::numeric <@ numrange(1.5, 3.5))::text", "true");
    // int4range with a plain-int probe is unaffected by the guard.
    // (PG rejects `int4range @> numeric` outright; SPG is leniently
    // permissive there, so we assert only the PG-valid plain-int form.)
    ck(&mut e, "('[1,10)'::int4range @> 5)::text", "true");
}

/// v7.37 D — percentile_cont(ARRAY[...]) returns a float array. PG18.4-verified.
#[test]
fn percentile_cont_array() {
    let mut e = Engine::new();
    let q = |e: &mut Engine, sql: &str| -> String {
        match e.execute(sql) {
            Ok(QueryResult::Rows { rows, .. }) => match &rows[0].values[0] {
                Value::Text(s) => s.to_string(), o => format!("{o:?}"),
            },
            Ok(o)=>format!("<NON:{o:?}>"), Err(e2)=>format!("ERR:{e2:?}"),
        }
    };
    assert_eq!(q(&mut e, "SELECT (percentile_cont(ARRAY[0.25,0.5,0.75]) WITHIN GROUP (ORDER BY v))::text FROM (VALUES (1.0),(2.0),(3.0),(4.0)) t(v)"), "{1.75,2.5,3.25}");
    assert_eq!(q(&mut e, "SELECT (percentile_cont(0.5) WITHIN GROUP (ORDER BY v))::text FROM (VALUES (1.0),(2.0),(3.0),(4.0)) t(v)"), "2.5");
}

/// v7.37 D — percentile_disc(ARRAY[...]) returns an array of the ordered-column
/// element type (one selected value per requested percentile). PG18.4-verified.
#[test]
fn percentile_disc_array() {
    let mut e = Engine::new();
    let q = |e: &mut Engine, sql: &str| -> String {
        match e.execute(sql) {
            Ok(QueryResult::Rows { rows, .. }) => match &rows[0].values[0] {
                Value::Text(s) => s.to_string(), o => format!("{o:?}"),
            },
            Ok(o)=>format!("<NON:{o:?}>"), Err(e2)=>format!("ERR:{e2:?}"),
        }
    };
    assert_eq!(q(&mut e, "SELECT (percentile_disc(ARRAY[0.25,0.5,0.75]) WITHIN GROUP (ORDER BY v))::text FROM (VALUES (10),(20),(30),(40)) t(v)"), "{10,20,30}");
    assert_eq!(q(&mut e, "SELECT (percentile_disc(ARRAY[0.5,1.0]) WITHIN GROUP (ORDER BY v))::text FROM (VALUES ('a'),('b'),('c')) t(v)"), "{b,c}");
    // scalar form unchanged
    assert_eq!(q(&mut e, "SELECT (percentile_disc(0.5) WITHIN GROUP (ORDER BY v))::text FROM (VALUES (1),(2),(3),(4)) t(v)"), "2");
}

/// v7.37 D — RANGE offset frame (`RANGE BETWEEN n PRECEDING AND n FOLLOWING`)
/// over a numeric ORDER BY column: ASC + DESC + mixed bounds. Output ordered by
/// the ORDER BY value so the comparison is deterministic. PG18.4-verified.
#[test]
fn range_offset_frame() {
    let mut e = Engine::new();
    let q = |e: &mut Engine, sql: &str| -> String {
        match e.execute(sql) {
            Ok(QueryResult::Rows { rows, .. }) => match &rows[0].values[0] {
                Value::Null=>"N".into(), Value::Text(s)=>s.to_string(), o=>format!("{o:?}"),
            },
            Ok(o)=>format!("<NON:{o:?}>"), Err(e2)=>format!("ERR:{e2:?}"),
        }
    };
    assert_eq!(q(&mut e, "SELECT string_agg(x,',' ORDER BY ord) FROM (SELECT v ord, (sum(v) OVER (ORDER BY v RANGE BETWEEN 1 PRECEDING AND 1 FOLLOWING))::text x FROM (VALUES (1),(2),(3),(5)) t(v)) s"), "3,6,5,5");
    assert_eq!(q(&mut e, "SELECT string_agg(x,',' ORDER BY ord) FROM (SELECT v ord, (sum(v) OVER (ORDER BY v DESC RANGE BETWEEN 2 PRECEDING AND CURRENT ROW))::text x FROM (VALUES (1),(2),(3),(5)) t(v)) s"), "6,5,8,5");
    assert_eq!(q(&mut e, "SELECT string_agg(x,',' ORDER BY ord) FROM (SELECT v ord, (sum(v) OVER (ORDER BY v RANGE BETWEEN 2 PRECEDING AND CURRENT ROW))::text x FROM (VALUES (1),(2),(3),(5)) t(v)) s"), "1,3,6,8");
    assert_eq!(q(&mut e, "SELECT string_agg(x,',' ORDER BY ord) FROM (SELECT v ord, (count(*) OVER (ORDER BY v RANGE BETWEEN CURRENT ROW AND 1 FOLLOWING))::text x FROM (VALUES (1),(2),(2),(4)) t(v)) s"), "3,2,2,1");
}

/// v7.37 D — GROUPS offset frame (`GROUPS BETWEEN n PRECEDING AND n FOLLOWING`)
/// counts peer groups. ASC + DESC + mixed bounds. Output ordered by the ORDER BY
/// value. PG18.4-verified.
#[test]
fn groups_offset_frame() {
    let mut e = Engine::new();
    let q = |e: &mut Engine, sql: &str| -> String {
        match e.execute(sql) {
            Ok(QueryResult::Rows { rows, .. }) => match &rows[0].values[0] {
                Value::Null=>"N".into(), Value::Text(s)=>s.to_string(), o=>format!("{o:?}"),
            },
            Ok(o)=>format!("<NON:{o:?}>"), Err(e2)=>format!("ERR:{e2:?}"),
        }
    };
    assert_eq!(q(&mut e, "SELECT string_agg(x,',' ORDER BY ord) FROM (SELECT v ord, (sum(v) OVER (ORDER BY v GROUPS BETWEEN 1 PRECEDING AND 1 FOLLOWING))::text x FROM (VALUES (1),(2),(2),(3),(5)) t(v)) s"), "5,8,8,12,8");
    assert_eq!(q(&mut e, "SELECT string_agg(x,',' ORDER BY ord) FROM (SELECT v ord, (count(*) OVER (ORDER BY v GROUPS BETWEEN 1 PRECEDING AND CURRENT ROW))::text x FROM (VALUES (1),(2),(2),(3)) t(v)) s"), "1,3,3,3");
    assert_eq!(q(&mut e, "SELECT string_agg(x,',' ORDER BY ord) FROM (SELECT v ord, (sum(v) OVER (ORDER BY v GROUPS BETWEEN CURRENT ROW AND 1 FOLLOWING))::text x FROM (VALUES (1),(2),(2),(3)) t(v)) s"), "5,7,7,3");
    assert_eq!(q(&mut e, "SELECT string_agg(x,',' ORDER BY ord) FROM (SELECT v ord, (count(*) OVER (ORDER BY v DESC GROUPS BETWEEN 1 PRECEDING AND 1 FOLLOWING))::text x FROM (VALUES (1),(2),(2),(3)) t(v)) s"), "3,4,4,3");
}

/// v7.37 D.19 — a bare (VALUES …) join operand must cross-join fully, not yield
/// only the first row. PG18.4-verified.
#[test]
fn join_bare_values_operand() {
    let mut e = Engine::new();
    let q = |e: &mut Engine, sql: &str| -> String {
        match e.execute(sql) {
            Ok(QueryResult::Rows { rows, .. }) => match &rows[0].values[0] {
                Value::Null=>"N".into(), Value::Text(s)=>s.to_string(), o=>format!("{o:?}") },
            Ok(o)=>format!("<{o:?}>"), Err(e2)=>format!("ERR:{e2:?}"),
        }
    };
    // CROSS JOIN two bare VALUES
    assert_eq!(q(&mut e, "SELECT string_agg(a.v||b.v, ',' ORDER BY a.v||b.v) FROM (VALUES ('a'),('b')) a(v) CROSS JOIN (VALUES ('1'),('2')) b(v)"), "a1,a2,b1,b2");
    // comma-join
    assert_eq!(q(&mut e, "SELECT string_agg(a.v||b.v, ',' ORDER BY a.v||b.v) FROM (VALUES ('a'),('b')) a(v), (VALUES ('1'),('2')) b(v)"), "a1,a2,b1,b2");
    // JOIN ... ON true
    assert_eq!(q(&mut e, "SELECT string_agg(a.v||b.v, ',' ORDER BY a.v||b.v) FROM (VALUES ('a'),('b')) a(v) JOIN (VALUES ('1'),('2')) b(v) ON true"), "a1,a2,b1,b2");
    // real table CROSS bare VALUES
    e.execute("CREATE TABLE ct19(v TEXT)").ok();
    e.execute("INSERT INTO ct19 VALUES ('a'),('b')").ok();
    assert_eq!(q(&mut e, "SELECT string_agg(ct19.v||b.v, ',' ORDER BY ct19.v||b.v) FROM ct19 CROSS JOIN (VALUES ('1'),('2')) b(v)"), "a1,a2,b1,b2");
    // regression guard: a genuinely correlated LATERAL still evaluates
    // per-left-row (must NOT take the new eager path).
    assert_eq!(q(&mut e, "SELECT string_agg(s.y, ',' ORDER BY s.y) FROM ct19 CROSS JOIN LATERAL (SELECT ct19.v || '!' AS y) s"), "a!,b!");
}

/// v7.37 D.21 — a correlated subquery in the projection/WHERE of a query whose
/// primary is a VALUES/subquery-derived table now resolves per-row (was
/// "engine resolver bug"). PG18.4-verified.
#[test]
fn correlated_subq_over_derived() {
    let mut e = Engine::new();
    let q = |e: &mut Engine, sql: &str| -> String {
        match e.execute(sql) {
            Ok(QueryResult::Rows { rows, .. }) => match &rows[0].values[0] {
                Value::Null=>"N".into(), Value::Text(s)=>s.to_string(), o=>format!("{o:?}") },
            Ok(o)=>format!("<{o:?}>"), Err(e2)=>format!("ERR:{e2:?}"),
        }
    };
    // correlated scalar subquery in projection over VALUES-derived outer
    assert_eq!(q(&mut e, "SELECT string_agg(g||':'||mx, ',' ORDER BY g) FROM (SELECT g, (SELECT max(v) FROM (VALUES (1,10),(1,20),(2,5)) u(gg,v) WHERE u.gg=t.g)::text mx FROM (VALUES (1),(2)) t(g)) s"), "1:20,2:5");
    // correlated in WHERE over derived outer
    assert_eq!(q(&mut e, "SELECT string_agg(g::text, ',' ORDER BY g) FROM (VALUES (1),(2),(3)) t(g) WHERE g < (SELECT max(v) FROM (VALUES (1),(3)) u(v) WHERE u.v <> t.g)"), "1,2");
    // EXISTS correlated over derived outer
    assert_eq!(q(&mut e, "SELECT string_agg(g::text, ',' ORDER BY g) FROM (VALUES (1),(2),(3)) t(g) WHERE EXISTS (SELECT 1 FROM (VALUES (2),(3)) u(v) WHERE u.v=t.g)"), "2,3");
}

/// v7.37 D.22 — a bare set-returning function in the SELECT projection (no FROM)
/// expands to rows, lowered to the equivalent FROM-position SRF. PG18.4-verified.
#[test]
fn srf_in_projection() {
    let mut e = Engine::new();
    let q = |e: &mut Engine, sql: &str| -> String {
        match e.execute(sql) {
            Ok(QueryResult::Rows { rows, .. }) => match &rows[0].values[0] {
                Value::Null=>"N".into(), Value::Text(s)=>s.to_string(), o=>format!("{o:?}") },
            Ok(o)=>format!("<{o:?}>"), Err(e2)=>format!("ERR:{e2:?}"),
        }
    };
    // bare unnest in projection (column named "unnest")
    assert_eq!(q(&mut e, "SELECT string_agg(unnest::text,',' ORDER BY unnest) FROM (SELECT unnest(ARRAY[5,3])) x"), "3,5");
    // aliased
    assert_eq!(q(&mut e, "SELECT string_agg(v::text,',' ORDER BY v) FROM (SELECT unnest(ARRAY[5,3]) v) x"), "3,5");
    // generate_series in projection
    assert_eq!(q(&mut e, "SELECT string_agg(generate_series::text,',' ORDER BY generate_series) FROM (SELECT generate_series(1,4)) x"), "1,2,3,4");
    // UNION of two projection-SRFs (the D.22 trigger)
    assert_eq!(q(&mut e, "SELECT string_agg(v::text,',' ORDER BY v) FROM (SELECT unnest(ARRAY[5,3]) v UNION SELECT unnest(ARRAY[3,1])) z"), "1,3,5");
}

/// v7.37 D.23 — window functions compose with GROUP BY aggregation: the
/// aggregate runs first, then window functions over the grouped rows.
/// PG18.4-verified.
#[test]
fn window_over_aggregate() {
    let mut e = Engine::new();
    let q = |e: &mut Engine, sql: &str| -> String {
        match e.execute(sql) {
            Ok(QueryResult::Rows { rows, .. }) => match &rows[0].values[0] {
                Value::Null=>"N".into(), Value::Text(s)=>s.to_string(), o=>format!("{o:?}") },
            Ok(o)=>format!("<{o:?}>"), Err(e2)=>format!("ERR:{e2:?}"),
        }
    };
    // rank() OVER (ORDER BY sum(v)) — window ORDER BY an aggregate
    assert_eq!(q(&mut e, "SELECT string_agg(g||':'||s||':'||rk, ',' ORDER BY g) FROM (SELECT g, sum(v) s, rank() OVER (ORDER BY sum(v) DESC) rk FROM (VALUES (1,10),(1,20),(2,5)) t(g,v) GROUP BY g) z"), "1:30:1,2:5:2");
    // plain aggregate + row_number window coexisting
    assert_eq!(q(&mut e, "SELECT string_agg(g||':'||rn, ',' ORDER BY g) FROM (SELECT g, sum(v), row_number() OVER (ORDER BY g) rn FROM (VALUES (1,10),(2,5),(3,7)) t(g,v) GROUP BY g) z"), "1:1,2:2,3:3");
    // window AGGREGATE over a plain aggregate (sum(count(*)) OVER ())
    assert_eq!(q(&mut e, "SELECT string_agg(g||':'||c||':'||tot, ',' ORDER BY g) FROM (SELECT g, count(*) c, sum(count(*)) OVER () tot FROM (VALUES (1,1),(1,1),(2,1)) t(g,v) GROUP BY g) z"), "1:2:3,2:1:3");
    // real table too
    e.execute("CREATE TABLE wtab(g INT, v INT)").unwrap();
    e.execute("INSERT INTO wtab VALUES (1,10),(1,20),(2,5)").unwrap();
    assert_eq!(q(&mut e, "SELECT string_agg(g||':'||rk, ',' ORDER BY g) FROM (SELECT g, rank() OVER (ORDER BY sum(v) DESC) rk FROM wtab GROUP BY g) z"), "1:1,2:2");
}

/// v7.37 D.24 — substring(string FROM pattern) POSIX regex extraction (no capture
/// group → whole match). Positional forms unchanged. PG18.4-verified.
#[test]
fn substring_regex_form() {
    let mut e = Engine::new();
    let q = |e: &mut Engine, sql: &str| -> String {
        match e.execute(sql) {
            Ok(QueryResult::Rows { rows, .. }) => match &rows[0].values[0] {
                Value::Null=>"NULL".into(), Value::Text(s)=>s.to_string(), o=>format!("{o:?}") },
            Ok(o)=>format!("<{o:?}>"), Err(_)=>"ERR".into(),
        }
    };
    assert_eq!(q(&mut e, "SELECT substring('hello world' FROM '[a-z]+')"), "hello");
    assert_eq!(q(&mut e, "SELECT COALESCE(substring('xyz' FROM '[0-9]+'), 'NULL')"), "NULL");
    assert_eq!(q(&mut e, "SELECT substring('id-4567-end' FROM '[0-9]+')"), "4567");
    assert_eq!(q(&mut e, "SELECT substring('abcdef' FROM 2 FOR 3)"), "bcd");
    assert_eq!(q(&mut e, "SELECT substring('abcdef', 2)"), "bcdef");
    assert_eq!(q(&mut e, "SELECT substring('foo@bar.com', '@[a-z.]+')"), "@bar.com");
    // capturing-group pattern → honest error (regex capture-extraction gap), not a
    // silent wrong result.
    assert_eq!(q(&mut e, "SELECT substring('hello world' FROM '(\\w+) (\\w+)')"), "ERR");
}

/// v7.37 D.25 — PG operator spellings of LIKE/ILIKE: ~~ / ~~* / !~~ / !~~*.
/// PG18.4-verified. Regex ~ still works (distinct operator).
#[test]
fn like_operators() {
    let mut e = Engine::new();
    let b = |e: &mut Engine, sql: &str| -> String {
        match e.execute(sql) {
            Ok(QueryResult::Rows { rows, .. }) => match &rows[0].values[0] {
                Value::Bool(v)=>v.to_string(), Value::Text(s)=>s.to_string(), Value::Null=>"N".into(), o=>format!("{o:?}") },
            Ok(_)=>"OK".into(), Err(_)=>"ERR".into(),
        }
    };
    assert_eq!(b(&mut e, "SELECT 'hello' ~~ 'h%'"), "true");
    assert_eq!(b(&mut e, "SELECT 'HELLO' ~~* 'h%'"), "true");
    assert_eq!(b(&mut e, "SELECT 'hello' !~~ 'x%'"), "true");
    assert_eq!(b(&mut e, "SELECT 'hello' !~~* 'X%'"), "true");
    assert_eq!(b(&mut e, "SELECT 'hello' ~~ 'H%'"), "false");
    assert_eq!(b(&mut e, "SELECT ('abc' ~ 'b')::text"), "true"); // regex ~ still distinct
    assert_eq!(b(&mut e, "SELECT string_agg(v, ',' ORDER BY v) FROM (VALUES ('apple'),('banana'),('avocado')) t(v) WHERE v ~~ 'a%'"), "apple,avocado");
}

/// v7.37 D.20 — a parenthesized set-op group or a CTE as a derived table.
/// PG18.4-verified. (Bare `(VALUES…) UNION` group left as follow-up.)
#[test]
fn derived_setop_group_and_cte() {
    let mut e = Engine::new();
    let q = |e: &mut Engine, sql: &str| -> String {
        match e.execute(sql) {
            Ok(QueryResult::Rows { rows, .. }) => match &rows[0].values[0] {
                Value::Null=>"N".into(), Value::Text(s)=>s.to_string(), o=>format!("{o:?}") },
            Ok(o)=>format!("<{o:?}>"), Err(e2)=>format!("ERR:{e2:?}"),
        }
    };
    // parenthesized set-op group as derived table
    assert_eq!(q(&mut e, "SELECT string_agg(x::text,',' ORDER BY x) FROM ((SELECT 1) UNION (SELECT 2)) s(x)"), "1,2");
    // 3-way group
    assert_eq!(q(&mut e, "SELECT string_agg(x::text,',' ORDER BY x) FROM ((SELECT 1) UNION (SELECT 2) UNION (SELECT 3)) s(x)"), "1,2,3");
    // CTE as derived table
    assert_eq!(q(&mut e, "SELECT string_agg(x,',' ORDER BY x) FROM (WITH RECURSIVE r(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM r WHERE n<4) SELECT n::text x FROM r) z"), "1,2,3,4");
    // regression: plain (SELECT ... UNION ...) without inner parens still works
    assert_eq!(q(&mut e, "SELECT string_agg(x::text,',' ORDER BY x) FROM (SELECT 1 UNION SELECT 2) s(x)"), "1,2");
}

/// v7.37 D.20 follow-up — a (VALUES…) list as a set-op group operand, as a
/// derived table. PG18.4-verified. Completes D.20.
#[test]
fn derived_values_group() {
    let mut e = Engine::new();
    let q = |e: &mut Engine, sql: &str| -> String {
        match e.execute(sql) {
            Ok(QueryResult::Rows { rows, .. }) => match &rows[0].values[0] {
                Value::Null=>"N".into(), Value::Text(s)=>s.to_string(), o=>format!("{o:?}") },
            Ok(o)=>format!("<{o:?}>"), Err(e2)=>format!("ERR:{e2:?}"),
        }
    };
    assert_eq!(q(&mut e, "SELECT string_agg(x::text,',' ORDER BY x) FROM ((VALUES (1),(2)) UNION (VALUES (2),(3))) s(x)"), "1,2,3");
    assert_eq!(q(&mut e, "SELECT string_agg(x::text,',' ORDER BY x) FROM ((VALUES (1),(2)) UNION ALL (VALUES (2),(3))) s(x)"), "1,2,2,3");
    assert_eq!(q(&mut e, "SELECT string_agg(x,',' ORDER BY x) FROM ((VALUES ('a')) UNION (SELECT 'b')) s(x)"), "a,b");
}

/// v7.37 D.22 follow-up — a set-returning function alongside scalar columns in a
/// no-FROM projection (`SELECT 'x', unnest(ARRAY[1,2])`) expands to rows with the
/// scalars repeated. PG18.4-verified.
#[test]
fn mixed_srf_in_projection() {
    let mut e = Engine::new();
    let q = |e: &mut Engine, sql: &str| -> String {
        match e.execute(sql) {
            Ok(QueryResult::Rows { rows, .. }) => match &rows[0].values[0] {
                Value::Null=>"N".into(), Value::Text(s)=>s.to_string(), o=>format!("{o:?}") },
            Ok(o)=>format!("<{o:?}>"), Err(e2)=>format!("ERR:{e2:?}"),
        }
    };
    // scalar + SRF, no FROM
    assert_eq!(q(&mut e, "SELECT string_agg(a||':'||u, ',' ORDER BY a, u) FROM (SELECT 'x' a, unnest(ARRAY[1,2]) u) s"), "x:1,x:2");
    // scalar + generate_series, no FROM
    assert_eq!(q(&mut e, "SELECT string_agg(a||':'||g, ',' ORDER BY g) FROM (SELECT 'y' a, generate_series(1,3) g) s"), "y:1,y:2,y:3");
    // bare single SRF still works (D.22 base case)
    assert_eq!(q(&mut e, "SELECT string_agg(u::text,',' ORDER BY u) FROM (SELECT unnest(ARRAY[5,3]) u) s"), "3,5");
    // mixed SRF over a real FROM still works (targetlist-SRF path, unchanged)
    e.execute("CREATE TABLE mt(id int, tags int[])").unwrap();
    e.execute("INSERT INTO mt VALUES (1, ARRAY[10,20]),(2,ARRAY[30])").unwrap();
    assert_eq!(q(&mut e, "SELECT string_agg(id||':'||u, ',' ORDER BY id, u) FROM (SELECT id, unnest(tags) u FROM mt) s"), "1:10,1:20,2:30");
}

/// v7.37 D.26 — count(col) over a VALUES/UNION-derived table excludes NULL rows.
/// The union result column is nullable when any branch is. PG18.4-verified.
#[test]
fn count_col_over_values_excludes_null() {
    let mut e = Engine::new();
    let q = |e: &mut Engine, sql: &str| -> String {
        match e.execute(sql) {
            Ok(QueryResult::Rows { rows, .. }) => match &rows[0].values[0] {
                Value::Null=>"N".into(), Value::Text(s)=>s.to_string(), o=>format!("{o:?}") },
            Ok(o)=>format!("<{o:?}>"), Err(e2)=>format!("ERR:{e2:?}"),
        }
    };
    // count(col) over VALUES with a NULL → 2 (was 3)
    assert_eq!(q(&mut e, "SELECT count(v)::text FROM (VALUES (1),(NULL),(3)) t(v)"), "2");
    // explicit UNION ALL with a NULL branch
    assert_eq!(q(&mut e, "SELECT count(v)::text FROM (SELECT 1 v UNION ALL SELECT NULL UNION ALL SELECT 3) t"), "2");
    // count(col) with no NULLs still all rows
    assert_eq!(q(&mut e, "SELECT count(v)::text FROM (VALUES (1),(2),(3)) t(v)"), "3");
    // real-table count(col) with NULL unchanged (was already correct)
    e.execute("CREATE TABLE ct2(v int)").unwrap();
    e.execute("INSERT INTO ct2 VALUES (1),(NULL),(3)").unwrap();
    assert_eq!(q(&mut e, "SELECT count(v)::text FROM ct2"), "2");
    // count(*) over VALUES counts all rows including NULL
    assert_eq!(q(&mut e, "SELECT count(*)::text FROM (VALUES (1),(NULL),(3)) t(v)"), "3");
}

/// v7.37 D.27 — an array-returning scalar subquery materialises (was "not yet
/// materialisable"). PG18.4-verified.
#[test]
fn array_scalar_subquery() {
    let mut e = Engine::new();
    let q = |e: &mut Engine, sql: &str| -> String {
        match e.execute(sql) {
            Ok(QueryResult::Rows { rows, .. }) => match &rows[0].values[0] {
                Value::Null=>"N".into(), Value::Text(s)=>s.to_string(), o=>format!("{o:?}") },
            Ok(o)=>format!("<{o:?}>"), Err(_)=>"ERR".into(),
        }
    };
    assert_eq!(q(&mut e, "SELECT (SELECT array_agg(v ORDER BY v) FROM (VALUES (3),(1),(2)) t(v))::text"), "{1,2,3}");
    assert_eq!(q(&mut e, "SELECT (SELECT array_agg(v ORDER BY v) FROM (VALUES ('b'),('a'),('c')) t(v))::text"), "{a,b,c}");
    assert_eq!(q(&mut e, "SELECT (SELECT array_agg(v) FROM (VALUES (1),(NULL),(3)) t(v))::text"), "{1,NULL,3}");
    assert_eq!(q(&mut e, "SELECT array_length((SELECT array_agg(v) FROM (VALUES (1),(2),(3)) t(v)), 1)::text"), "3");
}

/// v7.37 D.28 — a VIEW whose body has a `(VALUES …) t(cols)` derived table now
/// round-trips (view body stores the AST Display; the lateral_subquery Display
/// dropped the column aliases → ColumnNotFound on re-parse). PG18.4-verified.
#[test]
fn view_over_values_derived() {
    let mut e = Engine::new();
    let q = |e: &mut Engine, sql: &str| -> String {
        match e.execute(sql) {
            Ok(QueryResult::Rows { rows, .. }) => match &rows[0].values[0] {
                Value::Null=>"N".into(), Value::Text(s)=>s.to_string(), o=>format!("{o:?}") },
            Ok(o)=>format!("<{o:?}>"), Err(e2)=>format!("ERR:{e2:?}"),
        }
    };
    e.execute("CREATE VIEW vv AS SELECT g, g*10 AS d FROM (VALUES (1),(2),(3)) t(g)").unwrap();
    // top-level view query
    assert_eq!(q(&mut e, "SELECT string_agg(g||':'||d, ',' ORDER BY g) FROM vv WHERE d > 10"), "2:20,3:30");
    // aggregate over the view
    assert_eq!(q(&mut e, "SELECT count(*)::text FROM vv"), "3");
    // view inside a derived table + UNION
    assert_eq!(q(&mut e, "SELECT string_agg(x::text, ',' ORDER BY x) FROM (SELECT g x FROM vv UNION SELECT d FROM vv) u"), "1,2,3,10,20,30");
    // real LATERAL still round-trips (regression: parser now reads AS t(cols) after LATERAL)
    e.execute("CREATE TABLE lt(a int)").unwrap();
    e.execute("INSERT INTO lt VALUES (1),(2)").unwrap();
    assert_eq!(q(&mut e, "SELECT string_agg(a||':'||b, ',' ORDER BY a) FROM lt CROSS JOIN LATERAL (SELECT lt.a*10 b) s"), "1:10,2:20");
}

/// v7.37 D.29 — a view referenced inside an uncorrelated scalar / EXISTS / IN
/// subquery is now expanded (the subquery exec path used exec_bare_select_cancel,
/// which skips view/CTE/union expansion). PG18.4-verified.
#[test]
fn view_in_scalar_subquery() {
    let mut e = Engine::new();
    let q = |e: &mut Engine, sql: &str| -> String {
        match e.execute(sql) {
            Ok(QueryResult::Rows { rows, .. }) => match &rows[0].values[0] {
                Value::Null=>"N".into(), Value::Text(s)=>s.to_string(), o=>format!("{o:?}") },
            Ok(o)=>format!("<{o:?}>"), Err(e2)=>format!("ERR:{e2:?}"),
        }
    };
    e.execute("CREATE VIEW vw AS SELECT g, g*10 AS d FROM (VALUES (1),(2),(3)) t(g)").unwrap();
    assert_eq!(q(&mut e, "SELECT (SELECT count(*) FROM vw)::text"), "3");
    assert_eq!(q(&mut e, "SELECT (SELECT sum(d) FROM vw)::text"), "60");
    assert_eq!(q(&mut e, "SELECT (EXISTS (SELECT 1 FROM vw WHERE d > 25))::text"), "true");
    assert_eq!(q(&mut e, "SELECT string_agg(g::text,',' ORDER BY g) FROM vw WHERE d IN (SELECT d FROM vw WHERE g > 1)"), "2,3");
}

/// v7.37 D.30 — UPDATE ... FROM with a target-column reference in the SET RHS
/// (`SET v = v + u.bonus`) now resolves the target column instead of erroring.
/// PG18.4-verified.
#[test]
fn update_from_target_col_in_set() {
    let mut e = Engine::new();
    let q = |e: &mut Engine, sql: &str| -> String {
        match e.execute(sql) { Ok(QueryResult::Rows { rows, .. }) => match &rows[0].values[0] { Value::Text(s)=>s.to_string(), o=>format!("{o:?}") }, Ok(o)=>format!("<{o:?}>"), Err(e2)=>format!("ERR:{e2:?}") }
    };
    e.execute("CREATE TABLE t(id int primary key, v int, note text)").unwrap();
    e.execute("INSERT INTO t VALUES (1,10,'a'),(2,20,'b')").unwrap();
    e.execute("CREATE TABLE u(id int, bonus int)").unwrap();
    e.execute("INSERT INTO u VALUES (1,5),(2,7)").unwrap();
    e.execute("UPDATE t SET v = v + u.bonus FROM u WHERE u.id = t.id").unwrap();
    assert_eq!(q(&mut e, "SELECT string_agg(id||':'||v, ',' ORDER BY id) FROM t"), "1:15,2:27");
    e.execute("UPDATE t SET v = v * 2, note = note || u.bonus::text FROM u WHERE u.id = t.id").unwrap();
    assert_eq!(q(&mut e, "SELECT string_agg(id||':'||v||':'||note, ',' ORDER BY id) FROM t"), "1:30:a5,2:54:b7");
    e.execute("UPDATE t SET v = CASE WHEN u.bonus > 6 THEN v + 100 ELSE v END FROM u WHERE u.id = t.id").unwrap();
    assert_eq!(q(&mut e, "SELECT string_agg(id||':'||v, ',' ORDER BY id) FROM t"), "1:30,2:154");
    e.execute("UPDATE t SET v = GREATEST(v, u.bonus) FROM u WHERE u.id = t.id").unwrap();
    assert_eq!(q(&mut e, "SELECT string_agg(id||':'||v, ',' ORDER BY id) FROM t"), "1:30,2:154");
    // whole-RHS-is-source-col still works (regression)
    e.execute("UPDATE t SET v = u.bonus FROM u WHERE u.id = t.id").unwrap();
    assert_eq!(q(&mut e, "SELECT string_agg(id||':'||v, ',' ORDER BY id) FROM t"), "1:5,2:7");
}

/// v7.37 D.31 — numeric / numeric keeps PG's select_div_scale (≥16 sig digits)
/// instead of truncating to the operands' scale. PG18.4-verified.
#[test]
fn numeric_division_scale() {
    let mut e = Engine::new();
    let q = |e: &mut Engine, sql: &str| -> String {
        match e.execute(sql) { Ok(QueryResult::Rows { rows, .. }) => match &rows[0].values[0] { Value::Text(s)=>s.to_string(), o=>format!("{o:?}") }, Ok(o)=>format!("<{o:?}>"), Err(e2)=>format!("ERR:{e2:?}") }
    };
    let out = [
        q(&mut e, "SELECT (10::numeric / 3)::text"),
        q(&mut e, "SELECT (10::numeric / 3::numeric)::text"),
        q(&mut e, "SELECT (1::numeric / 7)::text"),
        q(&mut e, "SELECT (100::numeric / 4)::text"),
        q(&mut e, "SELECT (22.5::numeric / 1.5::numeric)::text"),
        q(&mut e, "SELECT (-10::numeric / 3)::text"),
        q(&mut e, "SELECT (0::numeric / 5)::text"),
        q(&mut e, "SELECT (123.456::numeric / 1)::text"),
        q(&mut e, "SELECT (avg(x))::text FROM (VALUES (10::numeric),(20),(31)) t(x)"),
        q(&mut e, "SELECT (1000000::numeric / 3)::text"),
    ];
    let exp = "3.3333333333333333|3.3333333333333333|0.14285714285714285714|25.0000000000000000|15.0000000000000000|-3.3333333333333333|0.00000000000000000000|123.4560000000000000|20.3333333333333333|333333.333333333333";
    assert_eq!(out.join("|"), exp);
}

/// v7.37 D.32 — EXTRACT accepts PG's plural field spellings. PG18.4-verified.
#[test]
fn extract_plural_fields() {
    let mut e = Engine::new();
    let q = |e: &mut Engine, sql: &str| -> String {
        match e.execute(sql) { Ok(QueryResult::Rows { rows, .. }) => match &rows[0].values[0] { Value::Text(s)=>s.to_string(), o=>format!("{o:?}") }, Ok(o)=>format!("<{o:?}>"), Err(e2)=>format!("ERR:{e2:?}") }
    };
    let out = [
        q(&mut e, "SELECT extract(years from interval '3 years')::text"),
        q(&mut e, "SELECT extract(months from interval '5 months')::text"),
        q(&mut e, "SELECT extract(days from interval '7 days')::text"),
        q(&mut e, "SELECT extract(hours from interval '4 hours')::text"),
        q(&mut e, "SELECT extract(minutes from interval '30 minutes')::text"),
        q(&mut e, "SELECT extract(weeks from date '2024-06-15')::text"),
        q(&mut e, "SELECT extract(decades from date '2024-06-15')::text"),
        q(&mut e, "SELECT extract(centuries from date '2024-06-15')::text"),
        q(&mut e, "SELECT extract(millenniums from date '2024-06-15')::text"),
        q(&mut e, "SELECT extract(day from interval '7 days')::text"),
        // date_part function form shares the plural aliases
        q(&mut e, "SELECT date_part('days', interval '7 days')::text"),
        q(&mut e, "SELECT date_part('months', interval '5 months')::text"),
    ];
    assert_eq!(out.join("|"), "3|5|7|4|30|24|202|21|3|7|7|5");
}

/// v7.37 D.34 — text || numeric (and numeric || text) is text concatenation,
/// not a rejected NUMERIC op. PG18.4-verified.
#[test]
fn text_numeric_concat() {
    let mut e = Engine::new();
    let q = |e: &mut Engine, sql: &str| -> String {
        match e.execute(sql) { Ok(QueryResult::Rows { rows, .. }) => match &rows[0].values[0] { Value::Text(s)=>s.to_string(), o=>format!("{o:?}") }, Ok(o)=>format!("<{o:?}>"), Err(e2)=>format!("ERR:{e2:?}") }
    };
    assert_eq!(q(&mut e, "SELECT ('x' || 1.5::numeric)"), "x1.5");
    assert_eq!(q(&mut e, "SELECT (1.5::numeric || 'x')"), "1.5x");
    assert_eq!(q(&mut e, "SELECT ('val=' || 3.14::numeric || '!')"), "val=3.14!");
    assert_eq!(q(&mut e, "SELECT ('n' || (10::numeric/4))"), "n2.5000000000000000");
    // int || numeric still fine (int coerces to text)
    assert_eq!(q(&mut e, "SELECT (5 || ':' || 2.5::numeric)"), "5:2.5");
}

/// v7.37 D.35 — date + time → timestamp (both operand orders). PG18.4-verified.
#[test]
fn date_plus_time_is_timestamp() {
    let mut e = Engine::new();
    let q = |e: &mut Engine, sql: &str| -> String {
        match e.execute(sql) { Ok(QueryResult::Rows { rows, .. }) => match &rows[0].values[0] { Value::Text(s)=>s.to_string(), o=>format!("{o:?}") }, Ok(o)=>format!("<{o:?}>"), Err(e2)=>format!("ERR:{e2:?}") }
    };
    assert_eq!(q(&mut e, "SELECT (date '2024-06-15' + time '10:30:00')::text"), "2024-06-15 10:30:00");
    assert_eq!(q(&mut e, "SELECT (time '23:59:59' + date '2024-06-15')::text"), "2024-06-15 23:59:59");
    assert_eq!(q(&mut e, "SELECT (date '2024-06-15' + time '00:00:00')::text"), "2024-06-15 00:00:00");
    // date + interval still works (regression)
    assert_eq!(q(&mut e, "SELECT (date '2024-06-15' + interval '1 day')::text"), "2024-06-16 00:00:00");
}

/// v7.37 D.36 — casting a non-text value to char(n)/varchar(n) stringifies first
/// (99::char(2) → '99'), instead of a CHAR/INT storage type-mismatch. PG18.4.
#[test]
fn value_to_charn_cast() {
    let mut e = Engine::new();
    let q = |e: &mut Engine, sql: &str| -> String {
        match e.execute(sql) { Ok(QueryResult::Rows { rows, .. }) => match &rows[0].values[0] { Value::Text(s)=>s.to_string(), o=>format!("{o:?}") }, Ok(o)=>format!("<{o:?}>"), Err(e2)=>format!("ERR:{e2:?}") }
    };
    assert_eq!(q(&mut e, "SELECT (99::char(2))"), "99");
    assert_eq!(q(&mut e, "SELECT (99::varchar(2))"), "99");
    assert_eq!(q(&mut e, "SELECT (12345::varchar(3))"), "123");
    assert_eq!(q(&mut e, "SELECT (3.14::varchar(10))"), "3.14");
    assert_eq!(q(&mut e, "SELECT (true::char(1))"), "t");
    assert_eq!(q(&mut e, "SELECT ('abcdef'::varchar(3))"), "abc");
}

/// v7.37 D.40 — FILTER (WHERE …) on a window aggregate restricts contributing
/// peer rows within the frame. PG18.4-verified.
#[test]
fn window_aggregate_filter() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t(g int, v int)").unwrap();
    e.execute("INSERT INTO t VALUES (1,10),(1,20),(1,20),(1,30),(2,5),(2,15)").unwrap();
    let q = |e: &mut Engine, sql: &str| -> String {
        match e.execute(sql) { Ok(QueryResult::Rows { rows, .. }) => match &rows[0].values[0] { Value::Text(s)=>s.to_string(), o=>format!("{o:?}") }, Ok(o)=>format!("<{o:?}>"), Err(e2)=>format!("ERR:{e2:?}") }
    };
    // count FILTER over whole partition
    assert_eq!(q(&mut e, "SELECT string_agg(v||':'||c, ',' ORDER BY v) FROM (SELECT v, count(*) FILTER (WHERE v > 15) OVER () c FROM t WHERE g=1) s"), "10:3,20:3,20:3,30:3");
    // sum FILTER over a running (ORDER BY) frame — excludes v=20 rows
    assert_eq!(q(&mut e, "SELECT string_agg(v||':'||s, ',' ORDER BY v) FROM (SELECT v, sum(v) FILTER (WHERE v <> 20) OVER (ORDER BY v) s FROM t WHERE g=1) x"), "10:10,20:10,20:10,30:40");
    // count FILTER over PARTITION BY
    assert_eq!(q(&mut e, "SELECT string_agg(g||':'||v||':'||c, ',' ORDER BY g,v) FROM (SELECT g, v, count(*) FILTER (WHERE v >= 15) OVER (PARTITION BY g) c FROM t) s"), "1:10:3,1:20:3,1:20:3,1:30:3,2:5:1,2:15:1");
    // min FILTER (exact — avg would hit the D.33 avg(int)→float divergence)
    assert_eq!(q(&mut e, "SELECT string_agg(v||':'||m, ',' ORDER BY v) FROM (SELECT v, min(v) FILTER (WHERE v > 10) OVER () m FROM t WHERE g=1) x"), "10:20,20:20,20:20,30:20");
    // plain OVER without FILTER unchanged (regression)
    assert_eq!(q(&mut e, "SELECT string_agg(v||':'||c, ',' ORDER BY v) FROM (SELECT v, count(*) OVER () c FROM t WHERE g=1) s"), "10:4,20:4,20:4,30:4");
}

/// v7.37 D.41 — SELECT DISTINCT dedups over a window projection. PG18.4-verified.
#[test]
fn distinct_over_window() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t(g int, v int)").unwrap();
    e.execute("INSERT INTO t VALUES (1,10),(1,20),(1,20),(2,5),(2,15),(3,7)").unwrap();
    let q = |e: &mut Engine, sql: &str| -> String {
        match e.execute(sql) { Ok(QueryResult::Rows { rows, .. }) => match &rows[0].values[0] { Value::Null=>"".into(), Value::Text(s)=>s.to_string(), o=>format!("{o:?}") }, Ok(o)=>format!("<{o:?}>"), Err(e2)=>format!("ERR:{e2:?}") }
    };
    assert_eq!(q(&mut e, "SELECT string_agg(x, ',' ORDER BY x) FROM (SELECT DISTINCT g||':'||(count(*) OVER (PARTITION BY g)) x FROM t) s"), "1:3,2:2,3:1");
    assert_eq!(q(&mut e, "SELECT (SELECT count(*) FROM (SELECT DISTINCT g, sum(v) OVER (PARTITION BY g) FROM t) s)::text"), "3");
    assert_eq!(q(&mut e, "SELECT string_agg(x, ',' ORDER BY x) FROM (SELECT DISTINCT (rank() OVER (ORDER BY g))::text x FROM t) s"), "1,4,6");
    // plain DISTINCT (no window) unchanged
    assert_eq!(q(&mut e, "SELECT (SELECT count(*) FROM (SELECT DISTINCT g FROM t) s)::text"), "3");
}

/// v7.37 D.42 — a multi-row VALUES seed recursive CTE terminates (non-recursive
/// UNION members are anchor terms, not recursive terms). PG18.4-verified.
#[test]
fn recursive_cte_multirow_seed() {
    let mut e = Engine::new();
    let q = |e: &mut Engine, sql: &str| -> String {
        match e.execute(sql) { Ok(QueryResult::Rows { rows, .. }) => match &rows[0].values[0] { Value::Text(s)=>s.to_string(), o=>format!("{o:?}") }, Ok(o)=>format!("<{o:?}>"), Err(e2)=>format!("ERR:{e2:?}") }
    };
    // multi-row VALUES seed (previously ran away to 100k iterations)
    assert_eq!(q(&mut e, "SELECT string_agg(n::text, ',' ORDER BY n) FROM (WITH RECURSIVE c(n) AS (VALUES(1),(2) UNION ALL SELECT n+10 FROM c WHERE n < 15) SELECT n FROM c) s"), "1,2,11,12,21,22");
    // single-row VALUES seed unchanged (task #387)
    assert_eq!(q(&mut e, "SELECT string_agg(n::text, ',' ORDER BY n) FROM (WITH RECURSIVE c(n) AS (VALUES(1) UNION ALL SELECT n+10 FROM c WHERE n < 15) SELECT n FROM c) s"), "1,11,21");
    // SELECT-based two-anchor seed
    assert_eq!(q(&mut e, "SELECT string_agg(n::text, ',' ORDER BY n) FROM (WITH RECURSIVE c(n) AS (SELECT 1 UNION SELECT 2 UNION ALL SELECT n+10 FROM c WHERE n < 15) SELECT DISTINCT n FROM c) s"), "1,2,11,12,21,22");
    // plain single-anchor recursion unchanged (regression)
    assert_eq!(q(&mut e, "SELECT string_agg(n::text, ',' ORDER BY n) FROM (WITH RECURSIVE c(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM c WHERE n < 5) SELECT n FROM c) s"), "1,2,3,4,5");
}

/// v7.37 D.43 — a WITH/RECURSIVE CTE in scalar-subquery position parses + runs.
/// PG18.4-verified.
#[test]
fn cte_in_scalar_subquery() {
    let mut e = Engine::new();
    let q = |e: &mut Engine, sql: &str| -> String {
        match e.execute(sql) { Ok(QueryResult::Rows { rows, .. }) => match &rows[0].values[0] { Value::Text(s)=>s.to_string(), o=>format!("{o:?}") }, Ok(o)=>format!("<{o:?}>"), Err(e2)=>format!("ERR:{e2:?}") }
    };
    assert_eq!(q(&mut e, "SELECT (WITH RECURSIVE c(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM c WHERE n < 100) SELECT count(*) FROM c)::text"), "100");
    assert_eq!(q(&mut e, "SELECT (WITH x AS (SELECT 5 v) SELECT v FROM x)::text"), "5");
    assert_eq!(q(&mut e, "SELECT (WITH RECURSIVE c(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM c WHERE n < 5) SELECT sum(n) FROM c)::text"), "15");
    // non-subquery parenthesised expression still works (regression)
    assert_eq!(q(&mut e, "SELECT ((1 + 2) * 3)::text"), "9");
}

/// v7.37 D.44 — MERGE with a `USING (SELECT …) alias` subquery source.
/// PG18.4-verified.
#[test]
fn merge_subquery_source() {
    let mut e = Engine::new();
    let agg = |e: &mut Engine, sql: &str| -> String {
        match e.execute(sql) { Ok(QueryResult::Rows { rows, .. }) => match rows.first().map(|r| &r.values[0]) { Some(Value::Text(s))=>s.to_string(), _=>"?".into() }, o=>format!("{o:?}") }
    };
    e.execute("CREATE TABLE tgt(id int primary key, v int)").unwrap();
    e.execute("INSERT INTO tgt VALUES (1,10),(2,20)").unwrap();
    e.execute("MERGE INTO tgt USING (SELECT * FROM (VALUES (1,100),(3,300)) s(id,v)) src ON tgt.id=src.id WHEN MATCHED THEN UPDATE SET v=src.v WHEN NOT MATCHED THEN INSERT (id,v) VALUES (src.id,src.v)").unwrap();
    assert_eq!(agg(&mut e, "SELECT string_agg(id||':'||v, ',' ORDER BY id) FROM tgt"), "1:100,2:20,3:300");
    e.execute("CREATE TABLE t2(id int primary key, v int)").unwrap();
    e.execute("INSERT INTO t2 VALUES (1,1),(2,2),(3,3)").unwrap();
    e.execute("MERGE INTO t2 USING (SELECT id, v*10 w FROM (VALUES (2,5),(4,7)) s(id,v)) src ON t2.id=src.id WHEN MATCHED THEN UPDATE SET v=src.w WHEN NOT MATCHED THEN INSERT (id,v) VALUES (src.id, src.w)").unwrap();
    assert_eq!(agg(&mut e, "SELECT string_agg(id||':'||v, ',' ORDER BY id) FROM t2"), "1:1,2:50,3:3,4:70");
    // plain table source unchanged (regression)
    e.execute("CREATE TABLE tg3(id int primary key, v int)").unwrap();
    e.execute("CREATE TABLE sr3(id int, v int)").unwrap();
    e.execute("INSERT INTO tg3 VALUES (1,10)").unwrap();
    e.execute("INSERT INTO sr3 VALUES (1,99),(2,88)").unwrap();
    e.execute("MERGE INTO tg3 USING sr3 ON tg3.id=sr3.id WHEN MATCHED THEN UPDATE SET v=sr3.v WHEN NOT MATCHED THEN INSERT (id,v) VALUES (sr3.id,sr3.v)").unwrap();
    assert_eq!(agg(&mut e, "SELECT string_agg(id||':'||v, ',' ORDER BY id) FROM tg3"), "1:99,2:88");
}

/// v7.37 D.45 — RANGE partitioning on INTEGER / DATE / BIGINT keys (not just
/// TIMESTAMPTZ). Routes INSERT/UPDATE by the key's own type. PG18.4-verified.
#[test]
fn range_partition_nontimestamp_key() {
    let mut e = Engine::new();
    let g = |e: &mut Engine, sql: &str| -> String { match e.execute(sql) { Ok(QueryResult::Rows { rows, .. }) => match rows.first().map(|r| &r.values[0]) { Some(Value::Text(s))=>s.to_string(), Some(Value::Null)=>"".into(), _=>"?".into() }, o=>format!("{o:?}") } };
    // INTEGER range key
    e.execute("CREATE TABLE mi(id int, g int) PARTITION BY RANGE (g)").unwrap();
    e.execute("CREATE TABLE mi_lo PARTITION OF mi FOR VALUES FROM (0) TO (10)").unwrap();
    e.execute("CREATE TABLE mi_hi PARTITION OF mi FOR VALUES FROM (10) TO (20)").unwrap();
    e.execute("CREATE TABLE mi_def PARTITION OF mi DEFAULT").unwrap();
    e.execute("INSERT INTO mi VALUES (1,5),(2,15),(3,8),(4,12),(5,25)").unwrap();
    assert_eq!(g(&mut e, "SELECT string_agg(id||':'||g,',' ORDER BY id) FROM mi_lo"), "1:5,3:8");
    assert_eq!(g(&mut e, "SELECT string_agg(id||':'||g,',' ORDER BY id) FROM mi_hi"), "2:15,4:12");
    assert_eq!(g(&mut e, "SELECT string_agg(id||':'||g,',' ORDER BY id) FROM mi_def"), "5:25");
    // DATE range key
    e.execute("CREATE TABLE md(id int, d date) PARTITION BY RANGE (d)").unwrap();
    e.execute("CREATE TABLE md_2025 PARTITION OF md FOR VALUES FROM ('2025-01-01') TO ('2026-01-01')").unwrap();
    e.execute("CREATE TABLE md_2026 PARTITION OF md FOR VALUES FROM ('2026-01-01') TO ('2027-01-01')").unwrap();
    e.execute("INSERT INTO md VALUES (1,'2025-06-15'),(2,'2026-03-20')").unwrap();
    assert_eq!(g(&mut e, "SELECT string_agg(id||':'||d,',' ORDER BY id) FROM md_2025"), "1:2025-06-15");
    assert_eq!(g(&mut e, "SELECT string_agg(id||':'||d,',' ORDER BY id) FROM md_2026"), "2:2026-03-20");
    // BIGINT range key
    e.execute("CREATE TABLE mb(id int, k bigint) PARTITION BY RANGE (k)").unwrap();
    e.execute("CREATE TABLE mb_a PARTITION OF mb FOR VALUES FROM (0) TO (1000000000000)").unwrap();
    e.execute("INSERT INTO mb VALUES (1, 500000000000)").unwrap();
    assert_eq!(g(&mut e, "SELECT string_agg(id||':'||k,',' ORDER BY id) FROM mb_a"), "1:500000000000");
}

/// v7.37 D.46 — DELETE on a partition parent fans out to children (RANGE + LIST).
/// PG18.4-verified.
#[test]
fn delete_on_partition_parent() {
    let mut e = Engine::new();
    let g = |e: &mut Engine, sql: &str| -> String { match e.execute(sql) { Ok(QueryResult::Rows { rows, .. }) => match rows.first().map(|r| &r.values[0]) { Some(Value::Text(s))=>s.to_string(), Some(Value::Null)=>"".into(), _=>"?".into() }, o=>format!("{o:?}") } };
    e.execute("CREATE TABLE mi(id int, g int) PARTITION BY RANGE (g)").unwrap();
    e.execute("CREATE TABLE mi_lo PARTITION OF mi FOR VALUES FROM (0) TO (10)").unwrap();
    e.execute("CREATE TABLE mi_hi PARTITION OF mi FOR VALUES FROM (10) TO (20)").unwrap();
    e.execute("INSERT INTO mi VALUES (1,5),(2,15),(3,8),(4,12)").unwrap();
    // DELETE on the parent removes matching rows from every child.
    let del = e.execute("DELETE FROM mi WHERE g < 10").unwrap();
    assert!(matches!(del, QueryResult::CommandOk { affected: 2, .. }), "{del:?}");
    assert_eq!(g(&mut e, "SELECT string_agg(id||':'||g,',' ORDER BY id) FROM mi"), "2:15,4:12");
    assert_eq!(g(&mut e, "SELECT string_agg(id||':'||g,',' ORDER BY id) FROM mi_lo"), "");
    // LIST parent DELETE fans out across the value + DEFAULT partitions.
    e.execute("CREATE TABLE l(id int, c text) PARTITION BY LIST (c)").unwrap();
    e.execute("CREATE TABLE l_a PARTITION OF l FOR VALUES IN ('a','b')").unwrap();
    e.execute("CREATE TABLE l_def PARTITION OF l DEFAULT").unwrap();
    e.execute("INSERT INTO l VALUES (1,'a'),(2,'z'),(3,'b'),(4,'y')").unwrap();
    e.execute("DELETE FROM l WHERE c IN ('a','z')").unwrap();
    assert_eq!(g(&mut e, "SELECT string_agg(id||':'||c,',' ORDER BY id) FROM l"), "3:b,4:y");
}

/// v7.37 D.47 (partial) — UPDATE on a partition parent fans out to children for
/// non-key SET lists; a key-touching UPDATE is rejected (row movement is a
/// follow-up). PG18.4-verified for the fan-out cases.
#[test]
fn update_on_partition_parent_nonkey() {
    let mut e = Engine::new();
    let g = |e: &mut Engine, sql: &str| -> String { match e.execute(sql) { Ok(QueryResult::Rows { rows, .. }) => match rows.first().map(|r| &r.values[0]) { Some(Value::Text(s))=>s.to_string(), Some(Value::Null)=>"".into(), _=>"?".into() }, o=>format!("{o:?}") } };
    e.execute("CREATE TABLE t(id int, g int, label text) PARTITION BY RANGE (g)").unwrap();
    e.execute("CREATE TABLE t_lo PARTITION OF t FOR VALUES FROM (0) TO (10)").unwrap();
    e.execute("CREATE TABLE t_hi PARTITION OF t FOR VALUES FROM (10) TO (20)").unwrap();
    e.execute("INSERT INTO t VALUES (1,5,'a'),(2,15,'b'),(3,8,'c')").unwrap();
    // non-key UPDATE with WHERE fans out and applies SET in each child
    let u1 = e.execute("UPDATE t SET label = upper(label) WHERE g < 10").unwrap();
    assert!(matches!(u1, QueryResult::CommandOk { affected: 2, .. }), "{u1:?}");
    assert_eq!(g(&mut e, "SELECT string_agg(id||':'||g||':'||label, ',' ORDER BY id) FROM t"), "1:5:A,2:15:b,3:8:C");
    // non-key UPDATE with no WHERE touches every child row
    e.execute("UPDATE t SET label = 'X'").unwrap();
    assert_eq!(g(&mut e, "SELECT string_agg(id||':'||label, ',' ORDER BY id) FROM t"), "1:X,2:X,3:X");
    // key-touching UPDATE on the parent is rejected honestly (not silently misfiled)
    assert!(e.execute("UPDATE t SET g = 17 WHERE id = 1").is_err());
    // the rejected UPDATE left the data untouched
    assert_eq!(g(&mut e, "SELECT string_agg(id||':'||g,',' ORDER BY id) FROM t"), "1:5,2:15,3:8");
}

/// v7.37 D.49 — jsonb_array_length / json_array_length accept a TEXT arg (PG
/// implicitly casts an unknown-type string literal to jsonb). PG18.4-verified.
#[test]
fn jsonb_array_length_text_arg() {
    let mut e = Engine::new();
    let q = |e: &mut Engine, sql: &str| -> String {
        match e.execute(sql) { Ok(QueryResult::Rows { rows, .. }) => format!("{:?}", &rows[0].values[0]), Ok(o)=>format!("{o:?}"), Err(er)=>format!("ERR:{:.30}", format!("{er:?}")) }
    };
    assert_eq!(q(&mut e, "SELECT jsonb_array_length('[1,2,3,4]')"), "Int(4)");
    assert_eq!(q(&mut e, "SELECT json_array_length('[1,2,3,4]')"), "Int(4)");
    // explicit ::jsonb still works
    assert_eq!(q(&mut e, "SELECT jsonb_array_length('[1,2,3,4]'::jsonb)"), "Int(4)");
    // NULL passthrough + non-array error preserved
    assert_eq!(q(&mut e, "SELECT jsonb_array_length(NULL)"), "Null");
    assert!(q(&mut e, "SELECT jsonb_array_length('{\"a\":1}')").starts_with("ERR"));
}

/// v7.37 D.49 (family) — jsonb_typeof / json_typeof / jsonb_strip_nulls also
/// accept a TEXT arg. PG18.4-verified.
#[test]
fn jsonb_typeof_strip_text_arg() {
    let mut e = Engine::new();
    let q = |e: &mut Engine, sql: &str| -> String {
        match e.execute(sql) { Ok(QueryResult::Rows { rows, .. }) => match &rows[0].values[0] { Value::Text(s)=>s.to_string(), Value::Null=>"".into(), o=>format!("{o:?}") }, Ok(_)=>"OK".into(), Err(er)=>format!("ERR:{:.25}", format!("{er:?}")) }
    };
    assert_eq!(q(&mut e, "SELECT jsonb_typeof('[1,2]')"), "array");
    assert_eq!(q(&mut e, "SELECT jsonb_typeof('{\"a\":1}')"), "object");
    assert_eq!(q(&mut e, "SELECT jsonb_typeof('42')"), "number");
    assert_eq!(q(&mut e, "SELECT json_typeof('\"hi\"')"), "string");
    // strip_nulls returns jsonb; its spacing is D.8-architectural so just check keys
    assert_eq!(q(&mut e, "SELECT (jsonb_strip_nulls('{\"a\":null,\"b\":1}')->>'b')"), "1");
    // NULL passthrough + ::jsonb still work
    assert_eq!(q(&mut e, "SELECT jsonb_typeof(NULL)"), "");
    assert_eq!(q(&mut e, "SELECT jsonb_typeof('[1]'::jsonb)"), "array");
}

/// v7.37 D.51 — to_tsquery accepts the `<->` adjacency operator (PG shorthand for
/// `<1>`). PG18.4-verified.
#[test]
fn tsquery_adjacency_operator() {
    let mut e = Engine::new();
    let q = |e: &mut Engine, sql: &str| -> String {
        match e.execute(sql) { Ok(QueryResult::Rows { rows, .. }) => match &rows[0].values[0] { Value::Text(s)=>s.to_string(), Value::Bool(b)=>format!("{b}"), o=>format!("{o:?}") }, Ok(_)=>"OK".into(), Err(er)=>format!("ERR:{:.30}", format!("{er:?}")) }
    };
    assert_eq!(q(&mut e, "SELECT (to_tsquery('english', 'quick <-> brown'))::text"), "'quick' <-> 'brown'");
    assert_eq!(q(&mut e, "SELECT (to_tsquery('english', 'quick <2> fox'))::text"), "'quick' <2> 'fox'");
    assert_eq!(q(&mut e, "SELECT (to_tsvector('english', 'the quick brown fox') @@ to_tsquery('english', 'quick <-> brown'))::text"), "true");
    assert_eq!(q(&mut e, "SELECT (to_tsvector('english', 'the quick brown fox') @@ to_tsquery('english', 'quick <-> fox'))::text"), "false");
    // <-> binds tighter than & (no stop words here to keep it about the operator)
    assert_eq!(q(&mut e, "SELECT (to_tsquery('english', 'foo <-> bar & baz'))::text"), "'foo' <-> 'bar' & 'baz'");
}

/// v7.37 D.53 — `UPDATE t SET arr[i] = v` array element assignment, PG NULL-pads
/// when i exceeds the array length. PG18.4-verified.
#[test]
fn update_set_array_element() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t(id int primary key, arr int[], tags text[])").unwrap();
    e.execute("INSERT INTO t VALUES (1, ARRAY[10,20,30], ARRAY['x','y']), (2, ARRAY[5], NULL)").unwrap();
    let g = |e: &mut Engine, sql: &str| -> String { match e.execute(sql) { Ok(QueryResult::Rows { rows, .. }) => match rows.first().map(|r| &r.values[0]) { Some(Value::Text(s))=>s.to_string(), Some(o)=>format!("{o:?}"), None=>"E".into() }, o=>format!("{o:?}") } };
    e.execute("UPDATE t SET arr[2] = 99 WHERE id=1").unwrap();
    assert_eq!(g(&mut e, "SELECT arr::text FROM t WHERE id=1"), "{10,99,30}");
    e.execute("UPDATE t SET tags[1] = 'z' WHERE id=1").unwrap();
    assert_eq!(g(&mut e, "SELECT tags::text FROM t WHERE id=1"), "{z,y}");
    // out-of-bounds NULL-pads
    e.execute("UPDATE t SET arr[10] = 7 WHERE id=2").unwrap();
    assert_eq!(g(&mut e, "SELECT arr::text FROM t WHERE id=2"), "{5,NULL,NULL,NULL,NULL,NULL,NULL,NULL,NULL,7}");
    // two element assignments in one SET
    e.execute("UPDATE t SET arr[1] = 100, arr[3] = 300 WHERE id=1").unwrap();
    assert_eq!(g(&mut e, "SELECT arr::text FROM t WHERE id=1"), "{100,99,300}");
}

/// v7.37 D.54 — IN/NOT IN subquery three-valued NULL logic. When the IN-list holds
/// a NULL and the LHS doesn't match, the predicate is UNKNOWN (NULL), not an error.
/// PG18.4-verified (the classic `x NOT IN (… NULL …)` gotcha).
#[test]
fn in_subquery_null_three_valued_logic() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t(id int, v int)").unwrap();
    e.execute("INSERT INTO t VALUES (1,10),(2,20),(3,NULL)").unwrap();
    e.execute("CREATE TABLE s(x int)").unwrap();
    e.execute("INSERT INTO s VALUES (10),(NULL)").unwrap();
    let q = |e: &mut Engine, sql: &str| -> String {
        match e.execute(sql) { Ok(QueryResult::Rows { rows, .. }) => match rows.first().map(|r| &r.values[0]) { Some(Value::Text(s))=>s.to_string(), Some(Value::Bool(b))=>format!("{b}"), Some(Value::Null)=>"".into(), Some(o)=>format!("{o:?}"), None=>"E".into() }, Ok(_)=>"OK".into(), Err(er)=>format!("ERR:{:.30}", format!("{er:?}")) }
    };
    let out = [
        q(&mut e, "SELECT string_agg(id::text, ',' ORDER BY id) FROM t WHERE v IN (SELECT x FROM s)"),
        q(&mut e, "SELECT coalesce(string_agg(id::text, ',' ORDER BY id),'(none)') FROM t WHERE v NOT IN (SELECT x FROM s)"),
        q(&mut e, "SELECT string_agg(id::text, ',' ORDER BY id) FROM t WHERE v IN (SELECT x FROM s WHERE x IS NOT NULL)"),
        q(&mut e, "SELECT coalesce(string_agg(id::text, ',' ORDER BY id),'(none)') FROM t WHERE v NOT IN (SELECT x FROM s WHERE x IS NOT NULL)"),
        q(&mut e, "SELECT (5 IN (SELECT x FROM s))::text"),
        q(&mut e, "SELECT (5 NOT IN (SELECT x FROM s))::text"),
        q(&mut e, "SELECT (10 IN (SELECT x FROM s))::text"),
    ];
    // PG18.4: a1=1 a2=(none) a3=1 a4=2 a5=NULL a6=NULL a7=true
    assert_eq!(out.join("|"), "1|(none)|1|2|||true");
}
