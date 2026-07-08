//! v7.38 (read01, T3.C3) — NUMERIC arithmetic beyond i128 (~38 digits) promotes
//! to arbitrary precision instead of erroring, and demotes back when a result
//! fits i128 again. Oracle: live PG 18.4.

use spg_engine::{Engine, QueryResult};

fn t(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => match &rows[0].values[0] {
            spg_storage::Value::Text(s) => s.to_string(),
            v => format!("{v:?}"),
        },
        _ => panic!("rows"),
    }
}

#[test]
fn numeric_beyond_i128() {
    let mut e = Engine::new();
    // 38-digit × 20-digit = 58-digit product.
    assert_eq!(
        t(&mut e, "SELECT (12345678901234567890123456789012345678 * 98765432109876543210)::text"),
        "1219326311370217952249657064224965706333485749112223746380"
    );
    // 39-digit × 2.
    assert_eq!(
        t(&mut e, "SELECT (123456789012345678901234567890123456789::numeric * 2)::text"),
        "246913578024691357802469135780246913578"
    );
    // Chained big op (product is big, then + 1) via a NumericBig operand.
    assert_eq!(
        t(&mut e, "SELECT (99999999999999999999999999999999999999 * 99999999999999999999999999999999999999 + 1)::text"),
        "9999999999999999999999999999999999999800000000000000000000000000000000000002"
    );
    // A result that fits i128 again stays a normal Numeric.
    assert_eq!(t(&mut e, "SELECT (10000000000000000000 * 2)::text"), "20000000000000000000");
    // A literal whose mantissa itself overflows i128 is now an exact NumericBig.
    assert_eq!(
        t(&mut e, "SELECT (123456789012345678901234567890123456789012345)::text"),
        "123456789012345678901234567890123456789012345"
    );
    assert_eq!(
        t(&mut e, "SELECT (99999999999999999999999999999999999999.5 + 0.5)::text"),
        "100000000000000000000000000000000000000.0"
    );
}


#[test]
fn numeric_bignum_compare() {
    let mut e = Engine::new();
    let b = |e: &mut Engine, sql: &str| matches!(
        e.execute(sql).unwrap(),
        QueryResult::Rows { ref rows, .. } if matches!(rows[0].values[0], spg_storage::Value::Bool(true))
    );
    // Big vs big (produced via arithmetic), and big vs a small int.
    assert!(b(&mut e, "SELECT (99999999999999999999999999999999999999 * 2) > (99999999999999999999999999999999999999 * 1)"));
    assert!(b(&mut e, "SELECT (99999999999999999999999999999999999999 * 2) = (99999999999999999999999999999999999999 + 99999999999999999999999999999999999999)"));
    assert!(!b(&mut e, "SELECT (99999999999999999999999999999999999999 * 2) < 5"));
}

#[test]
fn numeric_bignum_divide() {
    // v7.38 (read01, T3.C3) — division where an operand (or the scaled
    // quotient) exceeds i128 carries PG's display scale and half-away rounding.
    // Oracle: live PG 18.4 — byte-identical results.
    let mut e = Engine::new();
    // 76-digit product / 3 (exact).
    assert_eq!(
        t(&mut e, "SELECT (99999999999999999999999999999999999999 * 99999999999999999999999999999999999999 / 3)::text"),
        "3333333333333333333333333333333333333266666666666666666666666666666666666667"
    );
    // 45-digit / 7 — scale 0, rounds half away from zero (…620.71 → …621).
    assert_eq!(
        t(&mut e, "SELECT (123456789012345678901234567890123456789012345 / 7)::text"),
        "17636684144620811271604938270017636684144621"
    );
    // Big / big that lands back inside i128 demotes to a normal Numeric.
    assert_eq!(
        t(&mut e, "SELECT (99999999999999999999999999999999999999999 / 100000000000000000000)::text"),
        "1000000000000000000000"
    );
    // Modulo of a big integer.
    assert_eq!(
        t(&mut e, "SELECT (123456789012345678901234567890123456789012345 % 7)::text"),
        "5"
    );
    // Division by zero errors (matches PG).
    assert!(e
        .execute("SELECT 99999999999999999999999999999999999999 / 0")
        .is_err());
}

#[test]
fn numeric_bignum_order_by() {
    // v7.38 (read01, T3.C3) — ORDER BY / min / max over a NUMERIC column
    // holding values beyond i128 sorts by exact value, not by text or a lossy
    // f64. Includes a negated big literal (unary minus folds over NumericBig).
    // Oracle: live PG 18.4 — same sorted order.
    let mut e = Engine::new();
    e.execute("CREATE TABLE bo(x numeric)").unwrap();
    e.execute(
        "INSERT INTO bo VALUES \
         (123456789012345678901234567890123456789012345), (5), \
         (999999999999999999999999999999999999999999999), \
         ((-888888888888888888888888888888888888888888888))",
    )
    .unwrap();
    // ASC: -888… < 5 < 123… < 999…
    let order = match e.execute("SELECT x::text FROM bo ORDER BY x").unwrap() {
        QueryResult::Rows { rows, .. } => rows
            .iter()
            .map(|r| match &r.values[0] {
                spg_storage::Value::Text(s) => s.to_string(),
                v => format!("{v:?}"),
            })
            .collect::<Vec<_>>()
            .join(","),
        _ => panic!("rows"),
    };
    assert_eq!(
        order,
        "-888888888888888888888888888888888888888888888,\
         5,\
         123456789012345678901234567890123456789012345,\
         999999999999999999999999999999999999999999999"
    );
    // max / min pick the exact extremes (value_cmp path).
    assert_eq!(
        t(&mut e, "SELECT max(x)::text FROM bo"),
        "999999999999999999999999999999999999999999999"
    );
    assert_eq!(
        t(&mut e, "SELECT min(x)::text FROM bo"),
        "-888888888888888888888888888888888888888888888"
    );
}
