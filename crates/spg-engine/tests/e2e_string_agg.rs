//! PG `string_agg(expr, separator)` and `array_agg(expr)`.
//!
//! Reference:
//!   https://www.postgresql.org/docs/current/functions-aggregate.html
//!
//! Invariants pinned:
//!   * NULL inputs skipped (PG aggregate-skip-null behavior).
//!   * Result on empty group → NULL.
//!   * string_agg: needs 2 args (value, separator). separator
//!     inserted *between* values, not trailing.
//!   * array_agg returns NULL only when *all* inputs are NULL;
//!     otherwise it returns the array of non-NULL values
//!     in insertion order. Wait — PG actually keeps NULL elements:
//!       SELECT array_agg(x) FROM (VALUES (1),(NULL),(2)) v(x);
//!       => {1,NULL,2}
//!     and the result is NULL only when zero rows fed in.
//!   * Combined with GROUP BY.
//!
//! These are the ORM-report and Hibernate concat-rollup primitives.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn one_row(r: QueryResult) -> Vec<Value> {
    match r {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows.len(), 1);
            rows.into_iter().next().unwrap().values
        }
        _ => panic!(),
    }
}

fn engine_with_tags() -> Engine {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT NOT NULL, tag TEXT)")
        .unwrap();
    e.execute(
        "INSERT INTO t VALUES \
         (1, 'red'), (1, 'green'), (1, 'blue'), \
         (2, 'orange'), (2, 'yellow'), \
         (3, NULL), (3, 'cyan')",
    )
    .unwrap();
    e
}

// ── string_agg ───────────────────────────────────────────────────

#[test]
fn string_agg_two_arg_concat_with_separator() {
    let mut e = engine_with_tags();
    let r = e
        .execute("SELECT string_agg(tag, ',') FROM t WHERE id = 1")
        .unwrap();
    let row = one_row(r);
    // Order is INSERTion order in absence of ORDER BY inside the agg.
    assert_eq!(row[0], Value::Text("red,green,blue".into()));
}

#[test]
fn string_agg_skips_null_inputs() {
    let mut e = engine_with_tags();
    let r = e
        .execute("SELECT string_agg(tag, ',') FROM t WHERE id = 3")
        .unwrap();
    let row = one_row(r);
    // id=3 has (NULL, 'cyan'); NULL skipped, separator only between
    // surviving values → just "cyan".
    assert_eq!(row[0], Value::Text("cyan".into()));
}

#[test]
fn string_agg_empty_group_returns_null() {
    let mut e = engine_with_tags();
    let r = e
        .execute("SELECT string_agg(tag, ',') FROM t WHERE id = 99")
        .unwrap();
    let row = one_row(r);
    assert_eq!(row[0], Value::Null);
}

#[test]
fn string_agg_all_null_inputs_returns_null() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE n (v TEXT)").unwrap();
    e.execute("INSERT INTO n VALUES (NULL), (NULL)").unwrap();
    let r = e.execute("SELECT string_agg(v, ',') FROM n").unwrap();
    let row = one_row(r);
    assert_eq!(row[0], Value::Null);
}

#[test]
fn string_agg_single_value_no_separator() {
    // One surviving value → no separator at all.
    let mut e = Engine::new();
    e.execute("CREATE TABLE n (v TEXT)").unwrap();
    e.execute("INSERT INTO n VALUES ('only')").unwrap();
    let r = e.execute("SELECT string_agg(v, '|') FROM n").unwrap();
    let row = one_row(r);
    assert_eq!(row[0], Value::Text("only".into()));
}

#[test]
fn string_agg_with_group_by() {
    let mut e = engine_with_tags();
    let r = e
        .execute("SELECT id, string_agg(tag, ',') FROM t GROUP BY id ORDER BY id")
        .unwrap();
    let QueryResult::Rows { rows, .. } = r else {
        panic!()
    };
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0].values[0], Value::Int(1));
    assert_eq!(rows[0].values[1], Value::Text("red,green,blue".into()));
    assert_eq!(rows[1].values[0], Value::Int(2));
    assert_eq!(rows[1].values[1], Value::Text("orange,yellow".into()));
    assert_eq!(rows[2].values[0], Value::Int(3));
    assert_eq!(rows[2].values[1], Value::Text("cyan".into()));
}

#[test]
fn string_agg_arity_one_arg_errors() {
    let mut e = engine_with_tags();
    assert!(e.execute("SELECT string_agg(tag) FROM t").is_err());
}

#[test]
fn string_agg_arity_three_args_errors() {
    let mut e = engine_with_tags();
    // 3-arg form (with ORDER BY) not yet supported — must error,
    // not silently drop.
    assert!(e
        .execute("SELECT string_agg(tag, ',', 'x') FROM t")
        .is_err());
}

#[test]
fn string_agg_int_input_coerced_to_text() {
    // PG actually requires the input to be text; arg coercion is
    // the caller's job. Our implementation should at minimum
    // surface a clear type error rather than panic.
    let mut e = Engine::new();
    e.execute("CREATE TABLE n (v INT NOT NULL)").unwrap();
    e.execute("INSERT INTO n VALUES (1), (2)").unwrap();
    // PG: `string_agg(integer, text)` errors. We accept the cast
    // form `string_agg(v::text, ',')` as the canonical workaround.
    let r = e
        .execute("SELECT string_agg(v::text, ',') FROM n")
        .unwrap();
    let row = one_row(r);
    assert_eq!(row[0], Value::Text("1,2".into()));
}

// ── array_agg ────────────────────────────────────────────────────

#[test]
fn array_agg_text_returns_text_array() {
    let mut e = engine_with_tags();
    let r = e
        .execute("SELECT array_agg(tag) FROM t WHERE id = 1")
        .unwrap();
    let row = one_row(r);
    match &row[0] {
        Value::TextArray(items) => {
            let v: Vec<Option<String>> = items.clone();
            assert_eq!(
                v,
                vec![
                    Some("red".into()),
                    Some("green".into()),
                    Some("blue".into()),
                ]
            );
        }
        other => panic!("expected TextArray, got {other:?}"),
    }
}

#[test]
fn array_agg_keeps_nulls_in_array() {
    // PG: array_agg preserves NULL elements (different from
    // string_agg which skips them).
    let mut e = engine_with_tags();
    let r = e
        .execute("SELECT array_agg(tag) FROM t WHERE id = 3")
        .unwrap();
    let row = one_row(r);
    match &row[0] {
        Value::TextArray(items) => {
            assert_eq!(items.len(), 2);
            assert_eq!(items[0], None);
            assert_eq!(items[1], Some("cyan".into()));
        }
        other => panic!("expected TextArray, got {other:?}"),
    }
}

#[test]
fn array_agg_empty_group_returns_null() {
    let mut e = engine_with_tags();
    let r = e
        .execute("SELECT array_agg(tag) FROM t WHERE id = 99")
        .unwrap();
    let row = one_row(r);
    assert_eq!(row[0], Value::Null);
}

#[test]
fn array_agg_int_returns_int_array() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE n (v INT)").unwrap();
    e.execute("INSERT INTO n VALUES (3), (1), (4), (1), (5)")
        .unwrap();
    let r = e.execute("SELECT array_agg(v) FROM n").unwrap();
    let row = one_row(r);
    match &row[0] {
        Value::IntArray(items) => {
            assert_eq!(
                items,
                &vec![Some(3), Some(1), Some(4), Some(1), Some(5)]
            );
        }
        other => panic!("expected IntArray, got {other:?}"),
    }
}

#[test]
fn array_agg_bigint_returns_bigint_array() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE n (v BIGINT)").unwrap();
    e.execute("INSERT INTO n VALUES (10), (20)").unwrap();
    let r = e.execute("SELECT array_agg(v) FROM n").unwrap();
    let row = one_row(r);
    match &row[0] {
        Value::BigIntArray(items) => {
            assert_eq!(items, &vec![Some(10), Some(20)]);
        }
        other => panic!("expected BigIntArray, got {other:?}"),
    }
}

#[test]
fn array_agg_with_group_by() {
    let mut e = engine_with_tags();
    let r = e
        .execute("SELECT id, array_agg(tag) FROM t GROUP BY id ORDER BY id")
        .unwrap();
    let QueryResult::Rows { rows, .. } = r else {
        panic!()
    };
    assert_eq!(rows.len(), 3);
    if let Value::TextArray(items) = &rows[1].values[1] {
        assert_eq!(items.len(), 2);
        assert_eq!(items[0], Some("orange".into()));
        assert_eq!(items[1], Some("yellow".into()));
    } else {
        panic!()
    }
}

#[test]
fn array_agg_arity_zero_errors() {
    let mut e = engine_with_tags();
    assert!(e.execute("SELECT array_agg() FROM t").is_err());
}

#[test]
fn array_agg_arity_two_args_errors() {
    let mut e = engine_with_tags();
    // PG `array_agg(x, y)` doesn't exist; ORDER BY is a clause not
    // a positional arg.
    assert!(e.execute("SELECT array_agg(tag, id) FROM t").is_err());
}
