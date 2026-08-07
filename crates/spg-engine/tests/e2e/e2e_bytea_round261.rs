//! v7.39 (round 261) — the bytea surface, swept 81 cases against live
//! PG18.4 (2026-07-20). The function family was already solid (52/55 on
//! the first pass: the hex / base64 / escape codecs both ways,
//! get/set_byte, get/set_bit, substring / substr / position / overlay,
//! concatenation, the digest family, trims, bit_count, reverse,
//! comparison, convert_to / convert_from). Every gap was in INPUT
//! VALIDATION, and one of them accepted malformed data:
//!
//!   * `decode(…, 'base64')` stopped at the first `=` and decoded
//!     whatever it had, so an unpadded or wrong-length sequence came
//!     back as a value instead of an error — `decode('SGVsbG8','base64')`
//!     returned `\x48656c6c6f`. PG validates the sequence: whitespace is
//!     skipped anywhere, any other non-alphabet byte is `invalid symbol`,
//!     `=` may only close a group with at least two data characters, and
//!     the significant-character count must be a multiple of 4. A
//!     leading `=` returned EMPTY where PG raises.
//!   * `decode(…, 'hex')` only trimmed the ends, so an interior space
//!     was rejected — PG skips whitespace anywhere
//!     (`decode('41 42','hex')` is `\x4142`).
//!   * The digit / odd-length / unrecognized-encoding / malformed-escape
//!     errors all reported SPG's own wordings.
//!
//! PG uses one generic message for every malformed `escape` input
//! (`invalid input syntax for type bytea`), where SPG distinguished the
//! out-of-range octal from the bad sequence — probed, both collapse.

use spg_engine::{Engine, QueryResult};

fn one(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap_or_else(|x| panic!("{sql}: {x:?}")) {
        QueryResult::Rows { rows, .. } => spg_engine::eval::value_to_text(&rows[0].values[0]),
        other => panic!("{sql}: {other:?}"),
    }
}

fn err(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql) {
        Err(x) => format!("{x}"),
        Ok(ok) => panic!("{sql}: expected an error, got {ok:?}"),
    }
}

#[test]
fn base64_decoding_validates_the_sequence() {
    let mut e = Engine::new();
    // Well-formed lengths pass; whitespace is skipped anywhere.
    assert_eq!(
        one(&mut e, "SELECT decode('SGVsbG8=', 'base64')"),
        "\\x48656c6c6f"
    );
    assert_eq!(one(&mut e, "SELECT decode('SGVs', 'base64')"), "\\x48656c");
    assert_eq!(one(&mut e, "SELECT decode('', 'base64')"), "\\x");
    assert_eq!(
        one(&mut e, "SELECT decode('SGVs bG8=', 'base64')"),
        "\\x48656c6c6f"
    );
    // Wrong significant-character counts are refused — each of these
    // used to return a value.
    for sql in [
        "SELECT decode('SGVsbG8', 'base64')",
        "SELECT decode('SGVsbG', 'base64')",
        "SELECT decode('SG', 'base64')",
        "SELECT decode('S', 'base64')",
        "SELECT decode('SGVsbG8==', 'base64')",
    ] {
        let got = err(&mut e, sql);
        assert!(got.contains("invalid base64 end sequence"), "{sql} → {got}");
    }
    // A stray symbol names itself.
    let got = err(&mut e, "SELECT decode('SGVsbG8!', 'base64')");
    assert!(
        got.contains("invalid symbol \"!\" found while decoding base64 sequence"),
        "{got}"
    );
    // Padding in the wrong place — this returned EMPTY before.
    let got = err(&mut e, "SELECT decode('=SGVsbG8', 'base64')");
    assert!(
        got.contains("unexpected \"=\" while decoding base64 sequence"),
        "{got}"
    );
}

#[test]
fn hex_decoding_skips_whitespace_and_names_the_digit() {
    let mut e = Engine::new();
    assert_eq!(one(&mut e, "SELECT decode('4142', 'hex')"), "\\x4142");
    // An interior space is fine in PG; SPG used to reject it.
    assert_eq!(one(&mut e, "SELECT decode('41 42', 'hex')"), "\\x4142");
    assert_eq!(one(&mut e, "SELECT decode('', 'hex')"), "\\x");
    for (sql, want) in [
        (
            "SELECT decode('zz', 'hex')",
            "invalid hexadecimal digit: \"z\"",
        ),
        (
            "SELECT decode('4g', 'hex')",
            "invalid hexadecimal digit: \"g\"",
        ),
        (
            "SELECT decode('0x41', 'hex')",
            "invalid hexadecimal digit: \"x\"",
        ),
        (
            "SELECT decode('4', 'hex')",
            "invalid hexadecimal data: odd number of digits",
        ),
    ] {
        let got = err(&mut e, sql);
        assert!(got.contains(want), "{sql} → {got}");
    }
}

#[test]
fn escape_and_encoding_errors_take_pgs_wordings() {
    let mut e = Engine::new();
    assert_eq!(
        one(&mut e, "SELECT decode('Hello', 'escape')"),
        "\\x48656c6c6f"
    );
    assert_eq!(one(&mut e, "SELECT decode('\\\\', 'escape')"), "\\x5c");
    // One generic message covers every malformed escape input.
    for sql in [
        "SELECT decode('\\x41', 'escape')",
        "SELECT decode('\\41', 'escape')",
        "SELECT decode('\\', 'escape')",
    ] {
        let got = err(&mut e, sql);
        assert!(
            got.contains("invalid input syntax for type bytea"),
            "{sql} → {got}"
        );
    }
    let got = err(&mut e, "SELECT encode('\\x41'::bytea, 'nope')");
    assert!(got.contains("unrecognized encoding: \"nope\""), "{got}");
    let got = err(&mut e, "SELECT decode('41', 'nope')");
    assert!(got.contains("unrecognized encoding: \"nope\""), "{got}");
}

#[test]
fn the_bytea_core_is_unchanged() {
    let mut e = Engine::new();
    for (sql, want) in [
        ("SELECT '\\x48656c6c6f'::bytea", "\\x48656c6c6f"),
        ("SELECT length('\\x48656c6c6f'::bytea)", "5"),
        ("SELECT encode('\\x48656c6c6f'::bytea, 'hex')", "48656c6c6f"),
        (
            "SELECT encode('\\x48656c6c6f'::bytea, 'base64')",
            "SGVsbG8=",
        ),
        ("SELECT encode('\\x00ff41'::bytea, 'escape')", "\\000\\377A"),
        ("SELECT get_byte('\\x48656c6c6f'::bytea, 0)", "72"),
        (
            "SELECT set_byte('\\x48656c6c6f'::bytea, 0, 74)",
            "\\x4a656c6c6f",
        ),
        ("SELECT get_bit('\\x48'::bytea, 3)", "1"),
        (
            "SELECT substring('\\x48656c6c6f'::bytea from 2 for 3)",
            "\\x656c6c",
        ),
        (
            "SELECT position('\\x6c'::bytea in '\\x48656c6c6f'::bytea)",
            "3",
        ),
        (
            "SELECT overlay('\\x48656c6c6f'::bytea placing '\\x41'::bytea from 2)",
            "\\x48416c6c6f",
        ),
        (
            "SELECT '\\x4865'::bytea || '\\x6c6c6f'::bytea",
            "\\x48656c6c6f",
        ),
        (
            "SELECT btrim('\\x0048656c6c6f00'::bytea, '\\x00'::bytea)",
            "\\x48656c6c6f",
        ),
        ("SELECT bit_count('\\x48656c6c6f'::bytea)", "20"),
        ("SELECT reverse('\\x48656c6c6f'::bytea)", "\\x6f6c6c6548"),
        (
            "SELECT convert_from('\\x48656c6c6f'::bytea, 'UTF8')",
            "Hello",
        ),
        ("SELECT pg_typeof(get_byte('\\x41'::bytea, 0))", "integer"),
        // Round-trips.
        (
            "SELECT encode(decode('SGVsbG8=','base64'),'escape')",
            "Hello",
        ),
        ("SELECT encode('\\x41'::bytea, 'HEX')", "41"),
        ("SELECT encode('\\x41'::bytea, 'Base64')", "QQ=="),
    ] {
        assert_eq!(one(&mut e, sql), want, "{sql}");
    }
}
