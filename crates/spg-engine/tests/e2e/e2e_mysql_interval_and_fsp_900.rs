//! 9.0.0 — two MySQL-dialect divergences, measured on MySQL 9.7.2.
//!
//! **`INTERVAL n UNIT` is a *simple_expr*.** It is legal only as an
//! operand of `+` / `-` or as a function argument:
//!
//! ```text
//!   SELECT NOW() + INTERVAL 1 DAY          answers
//!   SELECT INTERVAL 1 DAY + NOW()          answers
//!   SELECT DATE_ADD(NOW(), INTERVAL 1 DAY) answers
//!   SELECT INTERVAL 1 DAY                  errno 1064
//!   SELECT (INTERVAL 1 DAY)                errno 1064
//!   SELECT 1 WHERE INTERVAL 1 DAY          errno 1064
//! ```
//!
//! SPG answered `1 day` to the bare one.
//!
//! **A `DATETIME(p)` fraction is padded to `p` digits**, and a cast's
//! own precision counts: `CAST('…05.090000' AS DATETIME(6))` prints
//! `…05.090000`, `DATETIME(3)` of `…05.000000` prints `…05.000`, and
//! `DATETIME(1)` of `…05` prints `…05.0`. SPG printed `…05.09`, `…05`
//! and `…05` — the padding machinery was there, but nothing carried the
//! CAST target's digits, so the walk went into the operand, which names
//! no column at all.

use spg_engine::Engine;

fn mysql() -> Engine {
    let mut e = Engine::new();
    e.set_mysql_dialect(true);
    e
}

#[test]
fn a_bare_interval_is_a_syntax_error_in_the_mysql_dialect() {
    let mut e = mysql();
    for sql in [
        "SELECT INTERVAL 1 DAY",
        "SELECT (INTERVAL 1 DAY)",
        "SELECT 1 WHERE INTERVAL 1 DAY",
    ] {
        let err = e.execute(sql).expect_err(sql);
        assert!(
            format!("{err:?}").contains("INTERVAL is only allowed"),
            "{sql}: {err:?}"
        );
    }
}

#[test]
fn an_interval_beside_an_operator_or_in_a_call_still_answers() {
    let mut e = mysql();
    // A literal stands in for NOW(): the clock functions want a wire
    // session, and the shape under test is the interval's POSITION.
    for sql in [
        "SELECT CAST('2020-01-02 03:04:05' AS DATETIME) + INTERVAL 1 DAY",
        "SELECT INTERVAL 1 DAY + CAST('2020-01-02 03:04:05' AS DATETIME)",
        "SELECT DATE_ADD(CAST('2020-01-02 03:04:05' AS DATETIME), INTERVAL 1 DAY)",
    ] {
        e.execute(sql)
            .unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    }
}

#[test]
fn postgresql_still_takes_a_bare_interval() {
    // The rule is MySQL's alone: `SELECT INTERVAL '1 day'` is an
    // ordinary value on PostgreSQL.
    let mut e = Engine::new();
    e.execute("SELECT INTERVAL '1 day'").expect("PG interval");
}

#[test]
fn a_cast_target_carries_its_own_fractional_digits() {
    // The projected column's declared precision is what the MySQL wire
    // pads to; this asserts the engine computes it from the CAST.
    let mut e = mysql();
    for (sql, want) in [
        (
            "SELECT CAST('2020-01-02 03:04:05.090000' AS DATETIME(6))",
            6,
        ),
        (
            "SELECT CAST('2020-01-02 03:04:05.000000' AS DATETIME(3))",
            3,
        ),
        ("SELECT CAST('2020-01-02 03:04:05' AS DATETIME(1))", 1),
    ] {
        let spg_engine::QueryResult::Rows { columns, .. } = e
            .execute(sql)
            .unwrap_or_else(|err| panic!("{sql}: {err:?}"))
        else {
            panic!("expected rows for {sql}");
        };
        assert_eq!(
            columns.first().and_then(|c| c.mysql_fsp),
            Some(want),
            "{sql}"
        );
    }
}
