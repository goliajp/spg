//! read01 round 471 (C13, type-fidelity epic P4b) — BIGINT UNSIGNED's
//! upper half.
//!
//! `BIGINT UNSIGNED` reaches 18446744073709551615; i64 stops at
//! 9223372036854775807. SPG stored the column as i64 and REFUSED anything
//! past that with `type mismatch in column "u": expected BIGINT, got
//! NUMERIC(0)` — so a MariaDB table with a real u64 in it could not be
//! loaded at all, and the error named a type the user never wrote.
//!
//! The epic's decision D9 called this: widen the storage tag to Numeric
//! (i128-backed, scale 0, which already compares, orders, indexes and
//! renders as an exact integer) and let the width marker keep the declared
//! type for SHOW CREATE and information_schema.
//!
//! Every expectation is copied from a MariaDB 11 run.

use spg_engine::{Engine, QueryResult};

fn my() -> Engine {
    let mut e = Engine::new();
    e.set_mysql_wire_session();
    e.execute("CREATE TABLE b (id INT, u BIGINT UNSIGNED, s BIGINT)")
        .unwrap();
    e.execute("INSERT INTO b VALUES (1, 18446744073709551615, 5)")
        .unwrap();
    e.execute("INSERT INTO b VALUES (2, 9223372036854775808, 5)")
        .unwrap();
    e.execute("INSERT INTO b VALUES (3, 9223372036854775807, 5)")
        .unwrap();
    e.execute("INSERT INTO b VALUES (4, 0, 5)").unwrap();
    e
}

fn one(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => rows
            .iter()
            .map(|r| {
                r.values
                    .iter()
                    .map(spg_engine::eval::value_to_text)
                    .collect::<Vec<_>>()
                    .join("|")
            })
            .collect::<Vec<_>>()
            .join(";"),
        other => panic!("{sql} -> {other:?}"),
    }
}

#[test]
fn round471_the_upper_half_stores_and_reads_back_exactly() {
    let mut e = my();
    assert_eq!(
        one(&mut e, "SELECT id, u FROM b ORDER BY id"),
        "1|18446744073709551615;2|9223372036854775808;3|9223372036854775807;4|0"
    );
}

#[test]
fn round471_arithmetic_and_aggregates_carry_the_full_range() {
    let mut e = my();
    // i64::MAX + 1 — the value that used to be `bigint out of range`.
    assert_eq!(
        one(&mut e, "SELECT u + 1 FROM b WHERE id=3"),
        "9223372036854775808"
    );
    assert_eq!(
        one(&mut e, "SELECT u - 1 FROM b WHERE id=1"),
        "18446744073709551614"
    );
    assert_eq!(
        one(&mut e, "SELECT MAX(u), MIN(u), SUM(u) FROM b"),
        "18446744073709551615|0|36893488147419103230"
    );
    // MariaDB gives division four decimal places, and so does Numeric.
    assert_eq!(
        one(&mut e, "SELECT u / 2 FROM b WHERE id=1"),
        "9223372036854775807.5000"
    );
}

#[test]
fn round471_comparison_and_ordering_are_unsigned() {
    // The failure mode a two's-complement representation would have had:
    // 18446744073709551615 sorting below 0.
    let mut e = my();
    assert_eq!(
        one(
            &mut e,
            "SELECT id FROM b WHERE u > 9223372036854775807 ORDER BY id"
        ),
        "1;2"
    );
    assert_eq!(one(&mut e, "SELECT id FROM b ORDER BY u DESC"), "1;2;3;4");
}

#[test]
fn round471_out_of_range_still_raises_with_mysqls_wording() {
    let mut e = my();
    for sql in [
        "INSERT INTO b VALUES (5, 18446744073709551616, 5)",
        "INSERT INTO b VALUES (6, -1, 5)",
    ] {
        let err = e.execute(sql).expect_err(&format!("{sql} must be refused"));
        assert!(
            format!("{err}").contains("Out of range value for column 'u'"),
            "{sql} gave: {err}"
        );
    }
}

#[test]
fn round471_it_still_reports_as_a_bigint() {
    // The storage tag moved to Numeric; every surface a client reads must
    // still say bigint, as MySQL does.
    // v7.39.2 — and without a display width: MySQL 9.7.2 writes
    // `bigint unsigned`, MariaDB `bigint(20) unsigned`, and SPG says it
    // is MySQL.
    let mut e = my();
    assert_eq!(
        one(
            &mut e,
            "SELECT column_type, data_type FROM information_schema.columns \
             WHERE table_name='b' AND column_name='u'"
        ),
        "bigint unsigned|bigint"
    );
    let create = one(&mut e, "SHOW CREATE TABLE b");
    assert!(
        create.contains("`u` bigint unsigned"),
        "SHOW CREATE said: {create}"
    );
    // A signed BIGINT is untouched by any of this.
    assert!(create.contains("`s` bigint "), "SHOW CREATE said: {create}");
}

#[test]
fn round471_the_unsigned_arithmetic_guard_still_sees_it() {
    // Round 467's range check reads the operands as integers; moving the
    // column to Numeric moved it out of that guard's reach until the guard
    // learned about scale-0 Numerics. MariaDB raises 1690 here.
    let mut e = my();
    assert!(
        e.execute("SELECT u - 18446744073709551615 - 1 FROM b WHERE id=4")
            .is_err(),
        "an unsigned underflow must still be refused"
    );
}
