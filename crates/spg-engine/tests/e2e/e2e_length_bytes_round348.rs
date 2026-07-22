//! read01 round 348 (MySQL differential, M3) — LENGTH counts bytes there.
//!
//! `LENGTH` is one name for two functions. MariaDB 11 counts BYTES —
//! `LENGTH('héllo')` is 6 and `LENGTH('日本語')` is 9 — while PG 18.4
//! counts characters and answers 5 and 3 (both measured). SPG answered
//! PG's number on a MySQL session, so a length check over multi-byte data
//! came back quietly short: no error, just a smaller number than the
//! client's own database gives. `CHAR_LENGTH` is the character count in
//! BOTH dialects and is untouched.
//!
//! MariaDB also applies LENGTH to a number through its string form
//! (`LENGTH(12345)` is 5); PG refuses it, and SPG refused it in both —
//! with `length() needs text or bytea, got Some(Int)`, a Rust Debug of an
//! internal shape. PG's own wording is
//! `function length(integer) does not exist`.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn row(e: &mut Engine, sql: &str) -> Vec<Value<'static>> {
    match e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err}")) {
        QueryResult::Rows { rows, .. } => rows
            .first()
            .map(|r| r.values.iter().cloned().map(Value::into_owned).collect())
            .unwrap_or_default(),
        other => panic!("`{sql}` did not return rows: {other:?}"),
    }
}

fn err(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql) {
        Ok(v) => panic!("{sql}: expected an error, got {v:?}"),
        Err(x) => format!("{x}"),
    }
}

fn mysql() -> Engine {
    let mut e = Engine::new();
    e.execute("SET sql_mode='STRICT_TRANS_TABLES'").unwrap();
    e
}

/// MariaDB's numbers, measured.
#[test]
fn mysql_length_counts_bytes() {
    let mut e = mysql();
    assert_eq!(
        row(&mut e, "SELECT LENGTH('héllo'), CHAR_LENGTH('héllo')"),
        vec![Value::Int(6), Value::Int(5)],
    );
    assert_eq!(
        row(&mut e, "SELECT LENGTH('日本語'), CHAR_LENGTH('日本語')"),
        vec![Value::Int(9), Value::Int(3)],
    );
    // ASCII is the same either way — that is why this hid so long.
    assert_eq!(row(&mut e, "SELECT LENGTH('abc')"), vec![Value::Int(3)]);
    assert_eq!(row(&mut e, "SELECT LENGTH('')"), vec![Value::Int(0)]);
    assert_eq!(row(&mut e, "SELECT LENGTH(NULL)"), vec![Value::Null]);
}

/// PG's numbers, measured — unchanged by this round.
#[test]
fn pg_length_counts_characters() {
    let mut e = Engine::new();
    assert_eq!(
        row(&mut e, "SELECT length('héllo'), char_length('héllo')"),
        vec![Value::Int(5), Value::Int(5)],
    );
    assert_eq!(row(&mut e, "SELECT length('日本語')"), vec![Value::Int(3)]);
}

/// The byte-counting spellings agree in both dialects and stay put.
#[test]
fn octet_and_bit_length_are_the_same_either_way() {
    for mut e in [Engine::new(), mysql()] {
        assert_eq!(
            row(&mut e, "SELECT OCTET_LENGTH('héllo'), BIT_LENGTH('héllo')"),
            vec![Value::Int(6), Value::Int(48)],
        );
        assert_eq!(
            row(&mut e, "SELECT CHARACTER_LENGTH('héllo')"),
            vec![Value::Int(5)],
        );
    }
}

/// A number: MySQL measures its string form, PG has no such function.
#[test]
fn a_number_argument_splits_by_dialect() {
    let mut m = mysql();
    assert_eq!(row(&mut m, "SELECT LENGTH(12345)"), vec![Value::Int(5)]);
    assert_eq!(row(&mut m, "SELECT LENGTH(1.50)"), vec![Value::Int(4)]);

    let mut p = Engine::new();
    assert_eq!(
        err(&mut p, "SELECT length(12345)"),
        "eval: type mismatch: function length(integer) does not exist",
    );
}
