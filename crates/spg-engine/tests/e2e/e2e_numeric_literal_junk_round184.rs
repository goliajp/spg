//! v7.39 (read01 round 184, A-usc) — numeric-literal trailing junk.
//!
//! PG's scanner rejects a numeric literal glued to identifier
//! characters; pre-r184 SPG lexed the number and silently turned the
//! tail into a column alias — `SELECT 12__34` returned 12. Live-PG18
//! differential (2026-07-18):
//!   12__34 / 123_ / 1.5_ / 1._5  → "trailing junk after numeric literal"
//!   0x (no digits)               → "invalid hexadecimal integer"
//! while 1_000, 1_000.5_5, 0x_F, 0xF_F, 1e1_0, 1_0e2, 1.e5 stay legal.

use spg_engine::{Engine, QueryResult};

fn val(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => format!("{:?}", rows[0].values[0]),
        other => panic!("{other:?}"),
    }
}

fn err(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql) {
        Err(err) => err.to_string(),
        Ok(r) => panic!("{sql:?} must error like PG, got {r:?}"),
    }
}

#[test]
fn trailing_junk_rejected() {
    let mut e = Engine::new();
    for sql in [
        "SELECT 12__34",
        "SELECT 123_",
        "SELECT 1.5_",
        "SELECT 1._5",
        "SELECT 123abc",
        "SELECT 0xF_",
        "SELECT 1e_5",
    ] {
        let m = err(&mut e, sql);
        assert!(
            m.contains("trailing junk after numeric literal")
                || m.contains("invalid number literal"),
            "{sql:?}: unexpected message {m:?}"
        );
    }
}

#[test]
fn empty_radix_rejected() {
    let mut e = Engine::new();
    let m = err(&mut e, "SELECT 0x");
    assert!(
        m.contains("invalid hexadecimal integer"),
        "0x: unexpected message {m:?}"
    );
    let m = err(&mut e, "SELECT 0o");
    assert!(m.contains("invalid octal integer"), "0o: {m:?}");
    let m = err(&mut e, "SELECT 0b");
    assert!(m.contains("invalid binary integer"), "0b: {m:?}");
    let m = err(&mut e, "SELECT 0x_");
    assert!(m.contains("invalid hexadecimal integer"), "0x_: {m:?}");
}

#[test]
fn legal_forms_unchanged() {
    let mut e = Engine::new();
    assert_eq!(val(&mut e, "SELECT 1_000"), "Int(1000)");
    assert_eq!(val(&mut e, "SELECT 0x1F"), "Int(31)");
    assert_eq!(val(&mut e, "SELECT 0x_F"), "Int(15)");
    assert_eq!(val(&mut e, "SELECT 0xF_F"), "Int(255)");
    assert_eq!(val(&mut e, "SELECT 0o17"), "Int(15)");
    assert_eq!(val(&mut e, "SELECT 0b101"), "Int(5)");
    // Aliases still work when separated by whitespace / AS.
    assert_eq!(val(&mut e, "SELECT 12 AS __34"), "Int(12)");
    assert_eq!(val(&mut e, "SELECT 1_000.5_5 = 1000.55"), "Bool(true)");
    assert_eq!(val(&mut e, "SELECT 1_0e2 = 1000"), "Bool(true)");
    assert_eq!(val(&mut e, "SELECT 1.e5 = 100000"), "Bool(true)");
}
