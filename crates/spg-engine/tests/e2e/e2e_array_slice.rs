//! Array slices arr[lo:hi] — PG 1-based inclusive, clamped
//! bounds, open ends.

use spg_engine::{Engine, QueryResult};

fn one(e: &mut Engine, sql: &str) -> spg_storage::Value<'static> {
    let r = e
        .execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    let QueryResult::Rows { rows, .. } = r else {
        panic!("expected Rows");
    };
    rows[0].values[0].clone()
}

fn ints(v: spg_storage::Value<'static>) -> Vec<Option<i32>> {
    match v {
        spg_storage::Value::IntArray(xs) => xs,
        other => panic!("expected int array, got {other:?}"),
    }
}

#[test]
fn slice_basic_and_open_ends() {
    let mut e = Engine::new();
    assert_eq!(
        ints(one(&mut e, "SELECT (ARRAY[10, 20, 30, 40])[2:3]")),
        [Some(20), Some(30)]
    );
    assert_eq!(
        ints(one(&mut e, "SELECT (ARRAY[10, 20, 30, 40])[:2]")),
        [Some(10), Some(20)]
    );
    assert_eq!(
        ints(one(&mut e, "SELECT (ARRAY[10, 20, 30, 40])[3:]")),
        [Some(30), Some(40)]
    );
}

#[test]
fn slice_clamps_and_empties() {
    let mut e = Engine::new();
    // Out-of-range bounds clamp (PG semantics).
    assert_eq!(
        ints(one(&mut e, "SELECT (ARRAY[1, 2])[0:99]")),
        [Some(1), Some(2)]
    );
    // Inverted window → empty array.
    assert_eq!(ints(one(&mut e, "SELECT (ARRAY[1, 2, 3])[3:1]")), []);
    // Plain subscript still works alongside.
    assert!(matches!(
        one(&mut e, "SELECT (ARRAY[7, 8])[2]"),
        spg_storage::Value::Int(8)
    ));
}

#[test]
fn slice_on_column_in_where() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE asl (tags TEXT[])").unwrap();
    e.execute("INSERT INTO asl VALUES (ARRAY['a','b','c'])")
        .unwrap();
    let v = one(&mut e, "SELECT tags[1:2] FROM asl");
    assert!(matches!(
        v,
        spg_storage::Value::TextArray(ref xs)
            if xs.len() == 2 && xs[1].as_deref() == Some("b")
    ));
}
