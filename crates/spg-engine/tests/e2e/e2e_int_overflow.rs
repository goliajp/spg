//! v7.38 (read01 P4.18) — INT_MIN / -1 and abs(INT_MIN) overflow with an
//! out-of-range error (never a panic, and never mis-reported as a division
//! by zero). Verified vs live PG 18.4, which raises "out of range".

use spg_engine::{Engine, QueryResult};

fn err_msg(e: &mut Engine, sql: &str) -> String {
    e.execute(sql).unwrap_err().to_string()
}

#[test]
fn int_min_div_and_abs_overflow_not_divzero() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE b(x bigint)").unwrap();
    // -9223372036854775808 (i64::MIN) computed, since the bare literal is a
    // separate lexer limitation.
    e.execute("INSERT INTO b VALUES (-9223372036854775807 - 1)").unwrap();

    // INT_MIN / -1 overflows — an out-of-range error, NOT "division by zero".
    let d = err_msg(&mut e, "SELECT x / (-1)::bigint FROM b");
    assert!(!d.contains("division by zero"), "got: {d}");
    assert!(d.contains("out of range") || d.contains("overflow"), "got: {d}");

    // abs(INT_MIN) overflows too (previously wrapped back to INT_MIN).
    assert!(err_msg(&mut e, "SELECT abs(x) FROM b").contains("out of range"));

    // A genuine zero divisor is still a division-by-zero error.
    assert!(err_msg(&mut e, "SELECT 5 / 0").contains("division by zero"));

    // Normal arithmetic is unaffected.
    assert!(matches!(e.execute("SELECT 10 / 3"), Ok(QueryResult::Rows { .. })));
    assert!(matches!(e.execute("SELECT abs(-5)"), Ok(QueryResult::Rows { .. })));
}
