//! 9.0.0 — a `B'101'` literal is PostgreSQL's FIXED-width `bit`, and its
//! column has no name.
//!
//! The parser routes the literal through an internal cast target, and
//! two things leaked from that:
//!
//! ```text
//!                             PG 18.6        SPG 8.0.4
//!   SELECT B'101' \gdesc      ?column?|"bit" __bit_literal|bit varying
//!   pg_typeof(B'101')         bit            bit varying
//!   pg_typeof(B'101'::bit(3)) bit            bit varying
//!   pg_typeof(B'101'::varbit) bit varying    bit varying
//! ```
//!
//! `__bit_literal` is a spelling of SPG's own reaching a client — the
//! same class as the `count_star` leak closed in v7.39.13. And `bit`
//! and `bit varying` are two types over one `Value::BitString`, so
//! which one it is has to come from the expression.

use spg_engine::{Engine, QueryResult};

fn one(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err}")) {
        QueryResult::Rows { rows, .. } => rows
            .first()
            .and_then(|r| r.values.first())
            .map(spg_engine::eval::value_to_text)
            .unwrap_or_default(),
        other => panic!("{sql}: {other:?}"),
    }
}

fn column_name(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err}")) {
        QueryResult::Rows { columns, .. } => columns[0].name.clone(),
        other => panic!("{sql}: {other:?}"),
    }
}

#[test]
fn a_bit_literal_names_no_column() {
    let mut e = Engine::new();
    assert_eq!(column_name(&mut e, "SELECT B'101'"), "?column?");
    // A written cast still names the type, as it does on PG.
    assert_eq!(column_name(&mut e, "SELECT B'101'::varbit"), "varbit");
}

#[test]
fn a_bit_literal_is_the_fixed_width_type() {
    let mut e = Engine::new();
    assert_eq!(one(&mut e, "SELECT pg_typeof(B'101')"), "bit");
    assert_eq!(one(&mut e, "SELECT pg_typeof(B'101'::bit(3))"), "bit");
    assert_eq!(
        one(&mut e, "SELECT pg_typeof(B'101'::varbit)"),
        "bit varying"
    );
}

/// The value is untouched: only which of the two types names it moved.
/// And Describe — what `\gdesc` and every driver read — says the same.
#[test]
fn describe_names_the_fixed_width_type_too() {
    let mut e = Engine::new();
    let ty = match e.execute("SELECT B'101'").unwrap() {
        QueryResult::Rows { columns, .. } => columns[0].ty,
        other => panic!("{other:?}"),
    };
    assert!(
        matches!(ty, spg_storage::DataType::Bit(_)),
        "PG 18.6 describes B'101' as \"bit\"; got {ty:?}"
    );
}

#[test]
fn the_bits_themselves_are_unchanged() {
    let mut e = Engine::new();
    assert_eq!(one(&mut e, "SELECT B'101'"), "101");
    assert_eq!(one(&mut e, "SELECT B'101' || B'0'"), "1010");
    assert_eq!(one(&mut e, "SELECT B'101'::int"), "5");
    assert_eq!(one(&mut e, "SELECT length(B'101')"), "3");
    assert_eq!(one(&mut e, "SELECT B'101' < B'110'"), "true");
}
