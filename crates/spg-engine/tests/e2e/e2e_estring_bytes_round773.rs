//! Round 773 (F31 July J3) — E-string byte escapes decode as BYTES
//! with whole-literal UTF-8 validation, PG18-measured: E'\303\251'
//! and E'\xC3\xA9' are both é (the old per-escape Latin-1 mapping
//! silently answered Ã© for every multi-byte sequence); E'\777'
//! masks to byte 0xFF and refuses with PG's encoding sentence.

use spg_engine::{Engine, QueryResult};

fn one(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => spg_engine::eval::value_to_text(&rows[0].values[0]),
        other => panic!("{other:?}"),
    }
}

#[test]
fn round773_byte_escapes_decode_as_pg() {
    let mut e = Engine::new();
    assert_eq!(one(&mut e, r"SELECT E'\303\251'"), "é");
    assert_eq!(one(&mut e, r"SELECT E'\xC3\xA9'"), "é");
    assert_eq!(one(&mut e, r"SELECT E'\101'"), "A");
    assert_eq!(one(&mut e, r"SELECT E'é'"), "é");
    assert_eq!(one(&mut e, r"SELECT E'a\tb'"), "a\tb");
    let err = format!(
        "{}",
        e.execute(r"SELECT E'\777'")
            .expect_err("0xFF byte must refuse")
    );
    assert!(
        err.contains("invalid byte sequence for encoding \"UTF8\": 0xff"),
        "{err}"
    );
}
