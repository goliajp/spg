//! v7.39 (round 273) — NUMERIC's declared scale may be negative.
//!
//! `numeric(10,-2)` rounds to hundreds and stores at display scale 0.
//! SPG rejected the column form and, worse, SILENTLY dropped the minus
//! out of a cast: `::numeric(10,-2)` reached the engine as the text
//! `numeric(10,2)` and rounded to two decimals instead — a wrong answer,
//! not an error.
//!
//! Every expectation was read off live PG 18.4.

use spg_engine::{Engine, QueryResult};

fn one(e: &mut Engine, sql: &str) -> String {
    let r = e
        .execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    let QueryResult::Rows { rows, .. } = r else {
        panic!("expected Rows from {sql}");
    };
    assert_eq!(rows.len(), 1, "{sql}");
    spg_engine::eval::value_to_text(&rows[0].values[0])
}

fn err(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql) {
        Ok(_) => panic!("{sql}: expected an error"),
        Err(x) => format!("{x}")
            .replace("eval: type mismatch: ", "")
            .replace("unsupported: ", ""),
    }
}

#[test]
fn a_negative_scale_rounds_to_that_power_of_ten() {
    let mut e = Engine::new();
    // Live PG 18.4, verbatim.
    assert_eq!(one(&mut e, "SELECT 1234.5678::numeric(10,-2)"), "1200");
    assert_eq!(one(&mut e, "SELECT 1250::numeric(10,-2)"), "1300");
    assert_eq!(one(&mut e, "SELECT 1350::numeric(10,-2)"), "1400");
    assert_eq!(one(&mut e, "SELECT -1250::numeric(10,-2)"), "-1300");
    assert_eq!(one(&mut e, "SELECT 1.5::numeric(10,-2)"), "0");
}

#[test]
fn the_rounding_happens_once() {
    let mut e = Engine::new();
    // 1249.5 to hundreds is 1200. Rounding to the integer first and then
    // to hundreds would round twice and give 1300.
    assert_eq!(one(&mut e, "SELECT 1249.5::numeric(10,-2)"), "1200");
    assert_eq!(one(&mut e, "SELECT 1250.4::numeric(10,-2)"), "1300");
}

#[test]
fn the_result_carries_display_scale_zero() {
    let mut e = Engine::new();
    assert_eq!(one(&mut e, "SELECT scale(1234.5::numeric(10,-2))"), "0");
    assert_eq!(
        one(&mut e, "SELECT pg_typeof(1234.5::numeric(10,-2))"),
        "numeric"
    );
    // It is an ordinary numeric afterwards.
    assert_eq!(one(&mut e, "SELECT 1234.5::numeric(10,-2) + 1"), "1201");
}

#[test]
fn the_precision_limit_widens_with_a_negative_scale() {
    let mut e = Engine::new();
    // precision 3, scale -2 permits 10^5. 12345 rounds to 12300, which
    // is three significant digits and fits.
    assert_eq!(one(&mut e, "SELECT 12345::numeric(3,-2)"), "12300");
    // 99999 rounds UP to 100000, which does not — the check is on the
    // value AFTER rounding, which is what PG's wording says.
    assert_eq!(
        err(&mut e, "SELECT 99999::numeric(3,-2)"),
        "numeric field overflow DETAIL: A field with precision 3, scale -2 must round to an \
         absolute value less than 10^5.",
    );
    assert!(e.execute("SELECT 100000::numeric(3,-2)").is_err());
}

#[test]
fn a_column_can_declare_one() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (a numeric(10,-2))").unwrap();
    e.execute("INSERT INTO t VALUES (12345.6)").unwrap();
    assert_eq!(one(&mut e, "SELECT a FROM t"), "12300");
}

#[test]
fn information_schema_reports_it_the_way_pg_does() {
    let mut e = Engine::new();
    e.execute(
        "CREATE TABLE t (a numeric(10,-2), b numeric(10,-5), c numeric(10,-1000), d numeric(10,3))",
    )
    .unwrap();
    // PG reports a negative declared scale as 2048 + scale here, which
    // three measured points confirm: -2 → 2046, -5 → 2043, -1000 → 1048.
    let r = e
        .execute(
            "SELECT column_name, numeric_precision, numeric_scale \
             FROM information_schema.columns WHERE table_name = 't' ORDER BY ordinal_position",
        )
        .unwrap();
    let QueryResult::Rows { rows, .. } = r else {
        panic!("expected Rows");
    };
    let got: Vec<String> = rows
        .iter()
        .map(|row| {
            row.values
                .iter()
                .map(|v| spg_engine::eval::value_to_text(v))
                .collect::<Vec<_>>()
                .join("|")
        })
        .collect();
    assert_eq!(got, vec!["a|10|2046", "b|10|2043", "c|10|1048", "d|10|3"]);
}

#[test]
fn the_scale_bounds_are_pgs() {
    let mut e = Engine::new();
    assert_eq!(
        err(&mut e, "SELECT 1.5::numeric(10,-1001)"),
        "NUMERIC scale -1001 must be between -1000 and 1000",
    );
    assert!(e.execute("CREATE TABLE t (a numeric(10,-1001))").is_err(),);
    // The legal extreme parses.
    e.execute("CREATE TABLE u (a numeric(10,-1000))").unwrap();
}
