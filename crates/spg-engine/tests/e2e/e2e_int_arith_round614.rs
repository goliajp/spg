//! v7.39 (round 614) — an integer addition built an error message on every
//! row that did not fail.
//!
//! A broad re-sweep put a plain arithmetic predicate at the top. On a
//! two-INT table of 500k rows, over pgwire against PG18:
//!
//!     WHERE id > 5        17.20 ms   PG 6.51    2.64x
//!     WHERE id + 1 > 5    72.04      PG 7.65    9.42x
//!     WHERE id * 2 > 5    71.91      PG 7.21    9.97x
//!     WHERE id % 3 = 0    72.80      PG 5.98   12.17x
//!
//! Four times the cost of a bare comparison for one integer operation, and
//! the allocating probe said one allocation a row — for an answer that is an
//! `i32`. Four mechanisms were proposed and each was measured and rejected:
//! the text cell being cloned when the row is materialised (the gap is the
//! same on a table with no text column), the VM's value stack (it is
//! caller-owned and recycled), the row itself (the scan borrows it from
//! storage), and `apply_binary`'s guard chain (skipping it with a by-ref
//! integer path moved 18.9 ms to 16.1, not to 4).
//!
//! A probe that returned a constant instead of calling `arith` dropped the
//! allocation to zero, which put it inside `arith` — where the integer arm
//! is this:
//!
//!     int_op(a, b).ok_or(EvalError::TypeMismatch {
//!         detail: format!("integer overflow on {op_name}"),
//!     })?
//!
//! `ok_or` takes its error BY VALUE, so the `format!` runs on every call,
//! including the overwhelming majority that return `Some`. Every arithmetic
//! operation in the engine was building — and immediately dropping — the
//! message for a failure that did not happen. Twenty-eight such sites became
//! `ok_or_else`; the error value is unchanged, only its construction is
//! deferred to the path that uses it.
//!
//!     WHERE id + 1 > 5   1 -> 0 allocations a row   18.9 -> 15.3 ms (200k)
//!     WHERE id % 3 = 0   1 -> 0                     19.2 -> 17.7
//!     count(id/7)        1 -> 0                     18.3 -> 12.1
//!     count(id::NUMERIC) 1 -> 0                     12.2 ->  9.9
//!
//! The last of those closes round 607's open item — "one allocation a row
//! remains on the named path, not located" — which was this, reached through
//! the cast's coercion.
//!
//! Over pgwire the reading is more mixed and is recorded as measured: the
//! pre-change binary is BIMODAL on this shape (six samples: 71.9, 72.1,
//! 73.1, 103.1, 107.1, 127.0) where the post-change one is not (ten samples
//! between 77.7 and 80.9). The median improves, 88.1 -> 78.0, and the spread
//! collapses; the best case is slightly worse. The engine-side probe, which
//! is the low-noise instrument, is unambiguous.
//!
//! `apply_binary_by_ref` also answers integer arithmetic now instead of
//! declining it, through the same `arith` / `div_op` / `mod_op` /
//! `int_div_op` calls the bottom of `apply_binary` makes — so every overflow
//! and divide-by-zero error is identical by construction. All 20 shapes here
//! were checked against live PG18 and matched byte for byte, the error
//! wordings included.

use spg_engine::{Engine, QueryResult};

fn vals(e: &mut Engine, sql: &str) -> Vec<String> {
    match e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err}")) {
        QueryResult::Rows { rows, .. } => rows
            .iter()
            .map(|r| {
                r.values
                    .iter()
                    .map(spg_engine::eval::value_to_text)
                    .collect::<Vec<_>>()
                    .join("|")
            })
            .collect(),
        other => panic!("{sql}: {other:?}"),
    }
}

fn err_of(e: &mut Engine, sql: &str) -> String {
    format!("{}", e.execute(sql).expect_err(sql))
}

/// The answers, including the sign rules division and modulo have to keep.
#[test]
fn round614_integer_arithmetic_answers() {
    let mut e = Engine::new();
    assert_eq!(
        vals(&mut e, "SELECT 7+3, 7-3, 7*3, 7/3, 7%3, -7/3, -7%3, 7/-3, 7 % -3"),
        vec!["10|4|21|2|1|-2|-1|-2|1"],
        "division truncates toward zero and the remainder takes the dividend's sign"
    );
    assert_eq!(
        vals(&mut e, "SELECT 1::SMALLINT+1::SMALLINT, 1::INT+1::BIGINT, 1::BIGINT+1::INT"),
        vec!["2|2|2"]
    );
    assert_eq!(
        vals(&mut e, "SELECT 32767::SMALLINT + 1::INT"),
        vec!["32768"],
        "mixing widths widens, so this one does NOT overflow"
    );
    assert_eq!(
        vals(&mut e, "SELECT NULL+1, 1+NULL, NULL/0, NULL%0"),
        vec!["NULL|NULL|NULL|NULL"],
        "a NULL operand answers NULL — even over a zero divisor"
    );
    assert_eq!(
        vals(&mut e, "SELECT 1.5+1, 1+1.5, 1.5::FLOAT8+1, 1::NUMERIC/3"),
        vec!["2.5|2.5|2.5|0.33333333333333333333"],
        "a non-integer operand keeps the general path"
    );
    assert_eq!(
        vals(&mut e, "SELECT '10'+2, 2+'10', '10'-2, '3'*2"),
        vec!["12|12|8|6"],
        "an unknown-type string literal still coerces to the integer's type"
    );
    assert_eq!(
        vals(&mut e, "SELECT 10/3, 10.0/3, 10::NUMERIC/3::NUMERIC"),
        vec!["3|3.3333333333333333|3.3333333333333333"]
    );
}

/// The errors, which are the whole reason the message was being built.
#[test]
fn round614_overflow_and_zero_divisor_errors() {
    let mut e = Engine::new();
    for (sql, want) in [
        ("SELECT 2147483647::INT + 1", "integer out of range"),
        ("SELECT (-2147483648)::INT - 1", "integer out of range"),
        ("SELECT 2147483647::INT * 2", "integer out of range"),
        ("SELECT 32767::SMALLINT + 1::SMALLINT", "smallint out of range"),
        ("SELECT 9223372036854775807::BIGINT + 1", "bigint out of range"),
        ("SELECT (-9223372036854775808)::BIGINT / (-1)", "bigint out of range"),
        ("SELECT 1/0", "division by zero"),
        ("SELECT 1%0", "division by zero"),
        ("SELECT 1::BIGINT/0", "division by zero"),
        ("SELECT 1::SMALLINT/0", "division by zero"),
        ("SELECT 5 % 0::SMALLINT", "division by zero"),
    ] {
        let got = err_of(&mut e, sql);
        assert!(
            got.contains(want),
            "{sql}: expected {want:?}, got {got:?} — these wordings are PG18's, \
             and deferring their construction must not change them"
        );
    }
}

/// Over rows, which is where the per-row message was being built.
#[test]
fn round614_over_rows() {
    let mut e = Engine::new();
    assert_eq!(
        vals(
            &mut e,
            "SELECT id, id+1, id-1, id*2, id/2, id%3 FROM (SELECT generate_series(1,5) id) q ORDER BY id"
        ),
        vec![
            "1|2|0|2|0|1",
            "2|3|1|4|1|2",
            "3|4|2|6|1|0",
            "4|5|3|8|2|1",
            "5|6|4|10|2|2",
        ]
    );
    assert_eq!(
        vals(&mut e, "SELECT count(*) FROM (SELECT generate_series(1,100) i) q WHERE i % 7 = 0"),
        vec!["14"]
    );
    assert_eq!(
        vals(&mut e, "SELECT count(*) FROM (SELECT generate_series(1,100) i) q WHERE i + 1 > 50"),
        vec!["51"]
    );
}

/// At the size where the message was built half a million times.
#[test]
fn round614_scale() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE big (id INT, g INT)").unwrap();
    e.execute("INSERT INTO big SELECT gg, gg % 50 FROM generate_series(1, 20000) gg")
        .unwrap();
    assert_eq!(
        vals(&mut e, "SELECT count(*) FROM big WHERE id + 1 > 5"),
        vec!["19996"]
    );
    assert_eq!(
        vals(&mut e, "SELECT count(*) FROM big WHERE id % 3 = 0"),
        vec!["6666"]
    );
    assert_eq!(
        vals(&mut e, "SELECT sum(id + g), sum(id * 2), sum(id / 2) FROM big"),
        vals(
            &mut e,
            "SELECT sum(id) + sum(g), 2 * sum(id), sum(id / 2) FROM big"
        ),
        "the row-by-row arithmetic agrees with the aggregate of it"
    );
    assert!(
        e.execute("SELECT count(*) FROM big WHERE id / (id - id) > 0").is_err(),
        "a divisor that becomes zero on the first row still raises"
    );
    assert_eq!(
        vals(&mut e, "SELECT count(*) FROM big WHERE id + 1 > 5 AND id > 0"),
        vec!["19996"],
        "two conjuncts, one fast-shaped and one not"
    );
}
