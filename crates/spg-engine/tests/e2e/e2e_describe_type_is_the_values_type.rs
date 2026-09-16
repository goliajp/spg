//! 8.0.3 — the type a result column announces is the type of the value
//! it carries.
//!
//! A computed expression was typed by describe's own rules (a binary
//! operator took its LEFT operand's type; a function the table did not
//! know became text) while the value came from the evaluator. A driver
//! decodes by the announced type, so in binary format every disagreement
//! was a wrong answer: psycopg read `SELECT 1 + 1.5` as the integer
//! 131072. 57 of 189 common expressions differed from PostgreSQL 18.6.
//!
//! Each case is asked three ways — Describe, the executed result's
//! column, and the value itself — and the expected type is PG's, measured
//! with `\gdesc` on 18.6.

use spg_engine::{Engine, QueryResult};
use spg_storage::DataType;

fn described(e: &Engine, sql: &str) -> DataType {
    let stmt = spg_sql::parser::parse_statement(sql).expect("parse");
    let (_, cols) = e.describe_prepared(&stmt);
    assert_eq!(cols.len(), 1, "{sql}");
    cols[0].ty
}

fn executed(e: &mut Engine, sql: &str) -> (DataType, Option<DataType>) {
    let QueryResult::Rows { columns, rows } = e.execute(sql).expect(sql) else {
        panic!("{sql}: no rows");
    };
    (columns[0].ty, rows[0].values[0].data_type())
}

fn numeric() -> DataType {
    DataType::Numeric {
        precision: 0,
        scale: 0,
    }
}

#[test]
fn a_computed_column_announces_the_type_of_its_value() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE dt (i INT, d DATE, f FLOAT8)")
        .unwrap();
    e.execute("INSERT INTO dt VALUES (1, '2026-01-02', 2.0)")
        .unwrap();
    let cases: [(&str, DataType); 7] = [
        ("SELECT 1 + 1.5", numeric()),
        ("SELECT i + 1.5 FROM dt", numeric()),
        ("SELECT d - DATE '2026-01-01' FROM dt", DataType::Int),
        ("SELECT 1 IS DISTINCT FROM 2", DataType::Bool),
        ("SELECT sqrt(2)", DataType::Float),
        ("SELECT power(2, 3)", DataType::Float),
        ("SELECT regexp_count('aaa', 'a')", DataType::Int),
    ];
    for (sql, want) in cases {
        let (column, value) = executed(&mut e, sql);
        let value = value.expect("non-NULL");
        assert!(
            core::mem::discriminant(&described(&e, sql)) == core::mem::discriminant(&want),
            "{sql}: Describe says {:?}, PG says {want:?}",
            described(&e, sql)
        );
        assert!(
            core::mem::discriminant(&column) == core::mem::discriminant(&want),
            "{sql}: the executed column says {column:?}, PG says {want:?}"
        );
        assert!(
            core::mem::discriminant(&value) == core::mem::discriminant(&want),
            "{sql}: the value is {value:?}, PG says {want:?}"
        );
    }
}

#[test]
fn a_type_the_value_cannot_tell_apart_keeps_the_rules_answer() {
    // timestamptz and timestamp share one value representation, as do
    // jsonb and json; the value alone cannot say which, the rules can.
    let mut e = Engine::new();
    assert_eq!(
        described(&e, "SELECT now() + interval '1 day'"),
        DataType::Timestamptz
    );
    assert_eq!(
        described(&e, "SELECT jsonb_build_object('a', 1)"),
        DataType::Jsonb
    );
    assert_eq!(
        executed(&mut e, "SELECT jsonb_build_object('a', 1)").0,
        DataType::Jsonb
    );
}
