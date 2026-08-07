//! v7.39 (round 615) — the exact standard deviation boxed four numbers for
//! every row.
//!
//! Since v7.38 the variance family keeps an EXACT Σx and Σx² alongside the
//! f64 pair, so an integer column answers with PG's numeric overload rather
//! than a double. That exactness is the feature, and it was costing nine
//! allocations a row on a plain INTEGER column — a boxed `BigNumeric` for
//! the input, one for its square, and a fresh box for each running total —
//! where `sum` and `avg` over the same column cost none:
//!
//!     sum(id)        0 allocations a row    3.6 ms   (200k rows)
//!     avg(id)        0                      3.7
//!     stddev(id)     9                     28.4
//!     variance(id)   9                     33.1
//!
//! `i128` holds the same integers exactly. An `int4` squares to at most
//! 4.6e18, so a running Σx² has room for 3.7e19 rows before it can overflow;
//! a `bigint` input that does overflow, or an exact input that is not an
//! integer, spends the fast pair into the BigNumeric accumulator and takes
//! the old path from there, so nothing is lost and no total is dropped.
//!
//!     stddev(id)     9 -> 0 allocations a row   28.4 -> 3.0 ms
//!     variance(id)   9 -> 0                     33.1 -> 3.2
//!     var_pop(id)    9 -> 0                     29.7 -> 3.8
//!
//! and over pgwire on 500k rows against PG18:
//!
//!     stddev(id)               63.37 -> 7.14   PG  5.36  10.44x -> 1.31x
//!     variance(id)             61.84 -> 6.92   PG  5.36  10.26x -> 1.29x
//!     stddev(id), variance(id) 123.23 -> 9.32  PG  5.45  20.12x -> 1.72x
//!     GROUP BY g, stddev(id)           8.51    PG 12.42            0.68x
//!
//! The exactness is what the pins are for: the answers have to be the ones
//! the BigNumeric path gave, digit for digit. All 18 shapes here were run
//! against the previous binary and are byte-identical — every width, a
//! NUMERIC input that falls back, a float input that takes the f64 overload,
//! a mixed group, both extremes of int4 and int8 (where the square is
//! largest), the samp / pop pair, one row, no rows, all-NULL, and a grouped
//! form where each group accumulates on its own.
//!
//! Recorded, not fixed, and older than this round (verified the same way):
//! SPG's exact `stddev` prints MORE digits than PG for some inputs —
//! `18918.32515364528` where PG writes `18918.32515365` — because the
//! square root's display scale is derived differently. `variance` agrees
//! exactly; it is the sqrt's scale alone.

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

fn seed() -> Engine {
    let mut e = Engine::new();
    e.execute("CREATE TABLE sv (g INT, i INT, b BIGINT, s SMALLINT, n NUMERIC, f FLOAT8)")
        .unwrap();
    e.execute(
        "INSERT INTO sv VALUES \
         (1, 1, 1, 1, 1.5, 1.5),(1, 2, 2, 2, 2.25, 2.25),(1, 3, 3, 3, 3.125, 3.125),\
         (2, 10, 9223372036854775807, 32767, 0.1, 0.1),\
         (2, -10, -9223372036854775808, -32768, -0.1, -0.1),\
         (3, 7, 7, 7, NULL, NULL),(3, NULL, NULL, NULL, 7, 7),(4, 5, 5, 5, 5, 5)",
    )
    .unwrap();
    e
}

/// Every width, and the six spellings of the family.
#[test]
fn round615_exact_over_every_integer_width() {
    let mut e = seed();
    assert_eq!(
        vals(
            &mut e,
            "SELECT stddev(i), variance(i), var_pop(i), stddev_pop(i), stddev_samp(i), var_samp(i) FROM sv"
        ),
        vec![
            "6.3471028261494461|40.2857142857142857|34.5306122448979592|\
             5.8762753717723235|6.3471028261494461|40.2857142857142857"
        ],
        "samp and pop differ in the divisor, and stddev is the sqrt of variance"
    );
    assert_eq!(
        vals(&mut e, "SELECT stddev(s), variance(s), var_pop(s) FROM sv"),
        vec!["18918.32515364528|357903026.61904762|306774022.81632653"],
        "SMALLINT, including both of its extremes"
    );
    assert_eq!(
        vals(&mut e, "SELECT stddev(b), variance(b) FROM sv"),
        vec!["5325116328314171700|28356863910078205285540093273695759027"],
        "BIGINT at both extremes — where the square is largest and the i128 \
         accumulator has to give way"
    );
    assert_eq!(
        vals(&mut e, "SELECT stddev(n), variance(n), var_pop(n) FROM sv"),
        vec!["2.5885335524927917|6.7005059523809524|5.7432908163265306"],
        "a NUMERIC input is exact but not an integer, so it spends the fast pair"
    );
    assert_eq!(
        vals(&mut e, "SELECT stddev(f), variance(f) FROM sv"),
        vec!["2.5885335524927924|6.700505952380955"],
        "a float input takes PG's float8 overload — a different answer from \
         the exact one just above, and deliberately so"
    );
}

/// The edges: one row, no rows, all NULL, and a constant column.
#[test]
fn round615_edges() {
    let mut e = seed();
    assert_eq!(
        vals(&mut e, "SELECT stddev(i) FROM sv WHERE g = 4"),
        vec!["NULL"],
        "samp needs two rows"
    );
    assert_eq!(
        vals(&mut e, "SELECT var_pop(i) FROM sv WHERE g = 4"),
        vec!["0"],
        "pop over one row is a bare 0, not a padded zero"
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT stddev(i) IS NULL, variance(i) IS NULL FROM sv WHERE g = 99"
        ),
        vec!["true|true"],
        "no rows"
    );
    assert_eq!(
        vals(&mut e, "SELECT stddev(i) FROM sv WHERE i IS NULL"),
        vec!["NULL"]
    );
    assert_eq!(
        vals(&mut e, "SELECT variance(v) FROM (VALUES (1),(1),(1)) t(v)"),
        vec!["0"],
        "a constant column has zero variance, spelled 0"
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT variance(v) FROM (VALUES (1::INT),(2),(4)) t(v)"
        ),
        vals(
            &mut e,
            "SELECT variance(v) FROM (VALUES (1::NUMERIC),(2),(4)) t(v)"
        ),
        "the i128 path and the BigNumeric path agree on the same numbers"
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT variance(v), stddev(v) FROM (VALUES (1::NUMERIC),(2),(4)) t(v)"
        ),
        vec!["2.3333333333333333|1.5275252316519467"]
    );
}

/// Each group accumulates on its own.
#[test]
fn round615_grouped() {
    let mut e = seed();
    assert_eq!(
        vals(
            &mut e,
            "SELECT g, stddev(i), variance(i), count(i) FROM sv GROUP BY g ORDER BY g"
        ),
        vec![
            "1|1.00000000000000000000|1.00000000000000000000|3",
            "2|14.1421356237309505|200.0000000000000000|2",
            "3|NULL|NULL|1",
            "4|NULL|NULL|1",
        ]
    );
}

/// The magnitudes where the i128 accumulator is doing the work, and the one
/// where it must hand over.
#[test]
fn round615_magnitudes() {
    let mut e = Engine::new();
    assert_eq!(
        vals(
            &mut e,
            "SELECT stddev(x), variance(x) FROM (SELECT generate_series(1,1000) x) q"
        ),
        vec!["288.8194360957494|83416.666666666667"]
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT stddev(x), variance(x) FROM (SELECT generate_series(1,1000)::BIGINT * 1000000000 x) q"
        ),
        vec!["288819436095.74939|83416666666666666666667"],
        "a billion times bigger, still exact"
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT stddev(v) FROM (VALUES (2147483647::INT),(2147483646),(2147483645)) t(v)"
        ),
        vec!["1.00000000000000000000"],
        "at int4's ceiling, where a float accumulator would lose the ones"
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT variance(v) FROM (VALUES (9223372036854775807::BIGINT),(9223372036854775806)) t(v)"
        ),
        vec!["0.50000000000000000000"],
        "at int8's ceiling — two squares that overflow i128 together, so this \
         is the hand-over, and it is still exact"
    );
}

/// At the size where the boxing was the cost.
#[test]
fn round615_scale() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE big (id INT, g INT)").unwrap();
    e.execute("INSERT INTO big SELECT gg, gg % 50 FROM generate_series(1, 20000) gg")
        .unwrap();
    assert_eq!(
        vals(&mut e, "SELECT variance(id) FROM big"),
        vec!["33335000.000000000000"]
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT count(*) FROM (SELECT g, stddev(id) FROM big GROUP BY g) q"
        ),
        vec!["50"]
    );
    assert_eq!(
        vals(&mut e, "SELECT variance(id) FROM big"),
        vals(&mut e, "SELECT variance(id::NUMERIC) FROM big"),
        "20000 rows through each path, digit for digit"
    );
}
