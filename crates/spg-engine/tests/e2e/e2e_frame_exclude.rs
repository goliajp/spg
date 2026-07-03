//! Window frame EXCLUDE {CURRENT ROW | NO OTHERS} — CURRENT ROW
//! drops the current row from the aggregate frame.

use spg_engine::{Engine, QueryResult};

fn col1(e: &mut Engine, sql: &str) -> Vec<i64> {
    let r = e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    let QueryResult::Rows { rows, .. } = r else {
        panic!("expected Rows");
    };
    rows.iter()
        .map(|row| match row.values[row.values.len() - 1] {
            spg_storage::Value::Int(n) => i64::from(n),
            spg_storage::Value::BigInt(n) => n,
            spg_storage::Value::Float(f) => f as i64,
            spg_storage::Value::Null => -1,
            ref other => panic!("unexpected {other:?}"),
        })
        .collect()
}

fn setup() -> Engine {
    let mut e = Engine::new();
    e.execute("CREATE TABLE fx (id INT, v INT)").unwrap();
    e.execute("INSERT INTO fx VALUES (1,10),(2,20),(3,30)").unwrap();
    e
}

#[test]
fn exclude_current_row() {
    let mut e = setup();
    // Whole-partition sum excluding the current row:
    // total 60, so each row sees 60 - its own value.
    let got = col1(
        &mut e,
        "SELECT id, sum(v) OVER (ORDER BY id ROWS BETWEEN UNBOUNDED PRECEDING \
         AND UNBOUNDED FOLLOWING EXCLUDE CURRENT ROW) FROM fx ORDER BY id",
    );
    assert_eq!(got, vec![50, 40, 30]);
}

#[test]
fn exclude_no_others_is_default() {
    let mut e = setup();
    // NO OTHERS is the plain frame — full total on every row.
    let got = col1(
        &mut e,
        "SELECT id, sum(v) OVER (ORDER BY id ROWS BETWEEN UNBOUNDED PRECEDING \
         AND UNBOUNDED FOLLOWING EXCLUDE NO OTHERS) FROM fx ORDER BY id",
    );
    assert_eq!(got, vec![60, 60, 60]);
}

#[test]
fn exclude_current_row_count() {
    let mut e = setup();
    // count(*) over the whole partition excluding self = 2 everywhere.
    let got = col1(
        &mut e,
        "SELECT id, count(*) OVER (ORDER BY id ROWS BETWEEN UNBOUNDED PRECEDING \
         AND UNBOUNDED FOLLOWING EXCLUDE CURRENT ROW) FROM fx ORDER BY id",
    );
    assert_eq!(got, vec![2, 2, 2]);
    // GROUP / TIES honestly error.
    assert!(e
        .execute(
            "SELECT sum(v) OVER (ORDER BY id ROWS UNBOUNDED PRECEDING EXCLUDE GROUP) FROM fx"
        )
        .is_err());
}
