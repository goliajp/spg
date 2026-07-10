//! v7.38 (T30) — convert_from/convert_to for the Windows code pages
//! WIN1250 / WIN1251 / WIN1253 / WIN1254. The high-half tables were extracted
//! byte-for-byte from live PG18.4 and verified: all 512 (byte, codepoint)
//! pairs match, and the 30 undefined bytes error in both. Oracle: PG18.4.

use spg_engine::{Engine, QueryResult};

fn ascii_of(e: &mut Engine, hex: &str, enc: &str) -> i64 {
    let sql = format!("SELECT ascii(convert_from('\\x{hex}'::bytea, '{enc}'))");
    match e
        .execute(&sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"))
    {
        QueryResult::Rows { rows, .. } => match &rows[0].values[0] {
            spg_storage::Value::Int(n) => i64::from(*n),
            spg_storage::Value::BigInt(n) => *n,
            other => panic!("expected int, got {other:?}"),
        },
        other => panic!("expected rows, got {other:?}"),
    }
}

#[test]
fn win125x_decode_matches_pg() {
    let mut e = Engine::new();
    // A representative byte from each page (verified vs PG18.4):
    // 1250 0x8A → Š(U+0160)=352 · 1251 0xC0 → А(U+0410)=1040 ·
    // 1253 0xC1 → Α(U+0391)=913 · 1254 0xDD → İ(U+0130)=304.
    assert_eq!(ascii_of(&mut e, "8a", "WIN1250"), 352);
    assert_eq!(ascii_of(&mut e, "c0", "WIN1251"), 1040);
    assert_eq!(ascii_of(&mut e, "c1", "WIN1253"), 913);
    assert_eq!(ascii_of(&mut e, "dd", "WIN1254"), 304);
    // The euro sign sits at 0x80 in 1250/1254, 0x88 in 1251, 0x80 in 1253.
    assert_eq!(ascii_of(&mut e, "80", "WIN1250"), 8364);
    assert_eq!(ascii_of(&mut e, "88", "WIN1251"), 8364);
    // WINDOWS<n> spelling is accepted too.
    assert_eq!(ascii_of(&mut e, "8a", "WINDOWS1250"), 352);
}

#[test]
fn win125x_round_trips() {
    // convert_to(convert_from(b)) restores the original byte for defined bytes.
    let mut e = Engine::new();
    let r = e
        .execute("SELECT convert_to(convert_from('\\x8a'::bytea, 'WIN1250'), 'WIN1250')")
        .unwrap();
    let QueryResult::Rows { rows, .. } = r else {
        panic!("rows")
    };
    assert_eq!(
        rows[0].values[0],
        spg_storage::Value::Bytes(vec![0x8a].into())
    );
}

#[test]
fn win125x_undefined_byte_errors_like_pg() {
    // 0x81 is undefined in WIN1250 — PG errors, so SPG must too (no silent
    // U+FFFD substitution).
    let mut e = Engine::new();
    assert!(
        e.execute("SELECT convert_from('\\x81'::bytea, 'WIN1250')")
            .is_err()
    );
    // A char with no WIN1253 (Greek) representation can't be encoded to it.
    assert!(e.execute("SELECT convert_to('А', 'WIN1253')").is_err());
}
