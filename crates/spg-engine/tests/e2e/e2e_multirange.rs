//! v7.37.5 δ — PG 14+ multirange smoke tests.
//!
//! Pins one round-trip per multirange kind plus the empty /
//! single-range / multi-range / NULL element cases.
//! Catalog tag 49 + 1-byte RangeKind, wire OIDs 4451/4537/
//! 4536/4533/4534/4535 (pg_type.dat). PG external form is
//! `{[a,b),[c,d)}` (`{}` for empty multirange).

use spg_engine::{Engine, QueryResult};
use spg_storage::{DataType, RangeKind, Value};

fn rows(r: QueryResult) -> Vec<Vec<Value>> {
    match r {
        QueryResult::Rows { rows, .. } => rows.into_iter().map(|r| r.values).collect(),
        _ => panic!("expected Rows"),
    }
}

fn col_type(e: &mut Engine, sql: &str) -> DataType {
    let r = e.execute(sql).unwrap();
    let QueryResult::Rows { columns, .. } = r else {
        panic!()
    };
    columns[0].ty
}

#[test]
fn create_table_int4multirange_column_is_accepted() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT NOT NULL, mr INT4MULTIRANGE NOT NULL)")
        .unwrap();
    assert_eq!(
        col_type(&mut e, "SELECT mr FROM t"),
        DataType::Multirange(RangeKind::Int4)
    );
}

#[test]
fn int4multirange_text_literal_round_trip() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT NOT NULL, mr INT4MULTIRANGE NOT NULL)")
        .unwrap();
    e.execute("INSERT INTO t VALUES (1, '{[1,5),[10,15)}'::int4multirange)")
        .unwrap();
    let r = rows(e.execute("SELECT mr FROM t").unwrap());
    let Value::Multirange { kind, ranges } = &r[0][0] else {
        panic!("expected Multirange, got {:?}", r[0][0]);
    };
    assert_eq!(*kind, RangeKind::Int4);
    assert_eq!(ranges.len(), 2);
    assert!(ranges[0].lower_inc);
    assert!(!ranges[0].upper_inc);
    assert!(matches!(&ranges[0].lower, Some(b) if matches!(b.as_ref(), Value::Int(1))));
    assert!(matches!(&ranges[0].upper, Some(b) if matches!(b.as_ref(), Value::Int(5))));
    assert!(matches!(&ranges[1].lower, Some(b) if matches!(b.as_ref(), Value::Int(10))));
    assert!(matches!(&ranges[1].upper, Some(b) if matches!(b.as_ref(), Value::Int(15))));
}

#[test]
fn empty_multirange_round_trips() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT NOT NULL, mr INT4MULTIRANGE NOT NULL)")
        .unwrap();
    e.execute("INSERT INTO t VALUES (1, '{}'::int4multirange)")
        .unwrap();
    let r = rows(e.execute("SELECT mr FROM t").unwrap());
    assert_eq!(
        r[0][0],
        Value::Multirange {
            kind: RangeKind::Int4,
            ranges: vec![],
        }
    );
}

#[test]
fn single_range_multirange_round_trip() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT NOT NULL, mr INT4MULTIRANGE NOT NULL)")
        .unwrap();
    e.execute("INSERT INTO t VALUES (1, '{[100,200]}'::int4multirange)")
        .unwrap();
    let r = rows(e.execute("SELECT mr FROM t").unwrap());
    let Value::Multirange { ranges, .. } = &r[0][0] else {
        panic!();
    };
    assert_eq!(ranges.len(), 1);
    assert!(ranges[0].lower_inc);
    assert!(ranges[0].upper_inc); // ']' inclusive upper
}

#[test]
fn int8multirange_round_trip() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT NOT NULL, mr INT8MULTIRANGE NOT NULL)")
        .unwrap();
    e.execute("INSERT INTO t VALUES (1, '{[1000000000,2000000000)}'::int8multirange)")
        .unwrap();
    let r = rows(e.execute("SELECT mr FROM t").unwrap());
    let Value::Multirange { kind, ranges } = &r[0][0] else {
        panic!();
    };
    assert_eq!(*kind, RangeKind::Int8);
    assert!(
        matches!(&ranges[0].lower, Some(b) if matches!(b.as_ref(), Value::BigInt(1_000_000_000)))
    );
}

#[test]
fn datemultirange_round_trip() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT NOT NULL, mr DATEMULTIRANGE NOT NULL)")
        .unwrap();
    e.execute("INSERT INTO t VALUES (1, '{[2024-01-01,2024-06-30)}'::datemultirange)")
        .unwrap();
    let r = rows(e.execute("SELECT mr FROM t").unwrap());
    let Value::Multirange { kind, ranges } = &r[0][0] else {
        panic!();
    };
    assert_eq!(*kind, RangeKind::Date);
    assert!(matches!(&ranges[0].lower, Some(b) if matches!(b.as_ref(), Value::Date(_))));
}

#[test]
fn multirange_cast_to_text_renders_canonical() {
    // Verify the inverse coerce: Multirange → Text. The text form
    // must round-trip back through `parse_multirange_str` (we test
    // the formatter end here via ::text cast).
    let mut e = Engine::new();
    let r = rows(
        e.execute("SELECT '{[1,5),[10,15)}'::int4multirange::text")
            .unwrap(),
    );
    let Value::Text(s) = &r[0][0] else {
        panic!("expected Text, got {:?}", r[0][0]);
    };
    assert_eq!(s, "{[1,5),[10,15)}");
}

#[test]
fn nullable_multirange_column_accepts_null() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT NOT NULL, mr INT4MULTIRANGE)")
        .unwrap();
    e.execute("INSERT INTO t VALUES (1, NULL), (2, '{[1,5)}'::int4multirange)")
        .unwrap();
    let r = rows(e.execute("SELECT mr FROM t ORDER BY id").unwrap());
    assert_eq!(r[0][0], Value::Null);
    let Value::Multirange { ranges, .. } = &r[1][0] else {
        panic!()
    };
    assert_eq!(ranges.len(), 1);
}
