//! v7.37.17 (17.6 siblings) — factorial + width_bucket.

use spg_engine::{Engine, QueryResult};

fn first(e: &mut Engine, sql: &str) -> spg_storage::Value<'static> {
    let r = e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    let QueryResult::Rows { rows, .. } = r else {
        panic!("expected Rows");
    };
    rows[0].values[0].clone()
}

#[test]
fn factorial_known_values() {
    let mut e = Engine::new();
    let cases = [
        (0i64, 1i64),
        (1, 1),
        (5, 120),
        (10, 3628800),
        (20, 2432902008176640000),
    ];
    for (n, expected) in cases {
        let sql = format!("SELECT factorial({n})");
        match first(&mut e, &sql) {
            spg_storage::Value::BigInt(v) => assert_eq!(v, expected, "factorial({n})"),
            other => panic!("factorial({n}): got {other:?}"),
        }
    }
}

#[test]
fn factorial_negative_errors() {
    let mut e = Engine::new();
    assert!(e.execute("SELECT factorial(-1)").is_err());
}

#[test]
fn factorial_overflow_errors() {
    let mut e = Engine::new();
    assert!(e.execute("SELECT factorial(21)").is_err());
}

#[test]
fn width_bucket_ascending() {
    let mut e = Engine::new();
    // width_bucket(op, low, high, count) — 4 equal buckets over [0,100]:
    //   bucket 1: [0, 25)
    //   bucket 2: [25, 50)
    //   bucket 3: [50, 75)
    //   bucket 4: [75, 100)
    //   below 0  → 0
    //   >= 100   → 5 (count+1)
    for (op, expected) in [
        (12.5, 1i32),
        (25.0, 2),
        (37.5, 2),
        (75.0, 4),
        (-1.0, 0),
        (100.0, 5),
        (200.0, 5),
    ] {
        let sql = format!("SELECT width_bucket({op}, 0.0, 100.0, 4)");
        match first(&mut e, &sql) {
            spg_storage::Value::Int(v) => assert_eq!(v, expected, "width_bucket({op})"),
            other => panic!("width_bucket({op}): got {other:?}"),
        }
    }
}

// read01 — the array form width_bucket(operand, thresholds[]) returns
// the count of ascending thresholds ≤ operand. Inline decimal arrays are
// inferred as TEXT[], so the thresholds are parsed numerically. vs PG18.4.
#[test]
fn width_bucket_array_form() {
    let mut e = Engine::new();
    let bucket = |e: &mut Engine, sql: &str| -> i64 {
        match first(e, sql) {
            spg_storage::Value::Int(n) => i64::from(n),
            other => panic!("{sql}: {other:?}"),
        }
    };
    assert_eq!(bucket(&mut e, "SELECT width_bucket(5, ARRAY[1,3,7,10])"), 2);
    assert_eq!(bucket(&mut e, "SELECT width_bucket(0, ARRAY[1,3,7,10])"), 0);
    assert_eq!(bucket(&mut e, "SELECT width_bucket(20, ARRAY[1,3,7,10])"), 4);
    assert_eq!(bucket(&mut e, "SELECT width_bucket(3, ARRAY[1,3,7,10])"), 2);
    assert_eq!(bucket(&mut e, "SELECT width_bucket(2.5, ARRAY[1.0,3.0,7.0])"), 1);
    // The 4-arg range form still works.
    assert_eq!(bucket(&mut e, "SELECT width_bucket(5.0, 0.0, 10.0, 5)"), 3);
}
