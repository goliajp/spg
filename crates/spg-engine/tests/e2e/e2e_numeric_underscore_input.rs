//! v7.38 (read01) — PG 16+ accepts `_` as a digit-group separator in numeric
//! text input (`'1_000'::numeric`), but only between two digits. Integer
//! input already accepted it; numeric input now does too. Oracle: live PG18.4.

use spg_engine::{Engine, QueryResult};

fn one(e: &mut Engine, sql: &str) -> String {
    match e
        .execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"))
    {
        QueryResult::Rows { rows, .. } => match &rows[0].values[0] {
            spg_storage::Value::Text(s) => s.to_string(),
            other => panic!("{sql}: expected Text, got {other:?}"),
        },
        other => panic!("{sql}: expected Rows, got {other:?}"),
    }
}

#[test]
fn numeric_input_accepts_between_digit_underscores() {
    let mut e = Engine::new();
    assert_eq!(one(&mut e, "SELECT ('1_000.5'::numeric)::text"), "1000.5");
    assert_eq!(one(&mut e, "SELECT ('1_000'::numeric)::text"), "1000");
    assert_eq!(one(&mut e, "SELECT ('1_0_0'::numeric)::text"), "100");
    assert_eq!(
        one(&mut e, "SELECT ('1_000.5_5'::numeric)::text"),
        "1000.55"
    );
    assert_eq!(one(&mut e, "SELECT ('1_000e3'::numeric)::text"), "1000000");
    // Integer input already handled it; keep it covered.
    assert_eq!(one(&mut e, "SELECT ('1_000'::int)::text"), "1000");
    assert_eq!(one(&mut e, "SELECT ('1_000'::bigint)::text"), "1000");
}

#[test]
fn numeric_input_rejects_misplaced_underscores() {
    // Leading / trailing / doubled / point-adjacent underscores are rejected,
    // like PG. (`1_000.` with a bare trailing point IS accepted by PG, so it is
    // not in this set.)
    let mut e = Engine::new();
    for lit in ["_1000", "1000_", "1__0", "1_.5", "._5"] {
        assert!(
            e.execute(&format!("SELECT '{lit}'::numeric")).is_err(),
            "{lit} must reject"
        );
    }
}
