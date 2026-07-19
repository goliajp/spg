//! v7.39 (round 272) — NUMERIC's declared precision runs to 1000.
//!
//! SPG capped it at 38, i128's width, so `numeric(50,10)` — a column PG
//! accepts — failed to parse. Values wider than i128 have had somewhere
//! to live since the arbitrary-precision form landed; only the declared
//! type could not say so.
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
fn a_precision_past_i128_is_accepted() {
    let mut e = Engine::new();
    assert_eq!(one(&mut e, "SELECT 1.5::numeric(50,10)"), "1.5000000000");
    // A 47-digit value going into numeric(50,2). This reported an
    // internal storage type mismatch, because casting the
    // arbitrary-precision form to a DECLARED numeric had no arm at all.
    assert_eq!(
        one(
            &mut e,
            "SELECT 12345678901234567890123456789012345678901234567.5::numeric(50,2)",
        ),
        "12345678901234567890123456789012345678901234567.50",
    );
}

#[test]
fn a_declared_scale_no_i128_can_hold_still_pads() {
    let mut e = Engine::new();
    // numeric(1000,999) is legal in PG. The rescale used to report
    // "overflow rescaling NUMERIC" for a typmod PG accepts.
    let got = one(&mut e, "SELECT 1.5::numeric(1000,999)");
    assert!(got.starts_with("1.5"), "{got}");
    assert_eq!(got.len(), "1.".len() + 999, "999 fractional digits");
    assert!(got[2..].bytes().skip(1).all(|b| b == b'0'), "{got}");
}

#[test]
fn the_typmod_bounds_are_pgs_and_are_reported_pgs_way() {
    let mut e = Engine::new();
    // The typmod used to be parsed as a u8 with unwrap_or(0), so an
    // out-of-range one silently became the UNCONSTRAINED type and the
    // cast quietly did nothing.
    assert_eq!(
        err(&mut e, "SELECT 1.5::numeric(1001)"),
        "NUMERIC precision 1001 must be between 1 and 1000",
    );
    assert_eq!(
        err(&mut e, "SELECT 1.5::numeric(0)"),
        "NUMERIC precision 0 must be between 1 and 1000",
    );
    assert_eq!(
        err(&mut e, "SELECT 1.5::numeric(10,1001)"),
        "NUMERIC scale 1001 must be between -1000 and 1000",
    );
}

#[test]
fn the_declared_precision_still_binds() {
    let mut e = Engine::new();
    // 51 integer digits into a field that allows 48 (50 - 2). The
    // i128 limit comparison cannot express a precision past 38, so the
    // check counts digits instead — without it a wide precision would
    // have accepted anything.
    assert_eq!(
        err(
            &mut e,
            "SELECT 123456789012345678901234567890123456789012345678901.5::numeric(50,2)",
        ),
        "numeric field overflow DETAIL: A field with precision 50, scale 2 must round to an \
         absolute value less than 10^48.",
    );
}

#[test]
fn a_wide_column_round_trips_and_enforces() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (a numeric(50,10))").unwrap();
    // 40 integer digits + 10 fractional = exactly 50.
    e.execute("INSERT INTO t VALUES (1234567890123456789012345678901234567890.1234567890)")
        .unwrap();
    assert_eq!(
        one(&mut e, "SELECT a FROM t"),
        "1234567890123456789012345678901234567890.1234567890",
    );
    // 41 integer digits does not fit, exactly as in PG.
    assert!(
        e.execute("INSERT INTO t VALUES (12345678901234567890123456789012345678901.1)")
            .is_err(),
    );
    assert_eq!(one(&mut e, "SELECT count(*) FROM t"), "1");
}

#[test]
fn information_schema_reports_the_wide_typmod() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (a numeric(50,10), b numeric(1000,500), c numeric)")
        .unwrap();
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
                .map(|v| match v {
                    spg_storage::Value::Null => String::new(),
                    other => spg_engine::eval::value_to_text(other),
                })
                .collect::<Vec<_>>()
                .join("|")
        })
        .collect();
    assert_eq!(got, vec!["a|50|10", "b|1000|500", "c||"]);
}
