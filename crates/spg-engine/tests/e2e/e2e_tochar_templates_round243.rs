//! v7.39 (round 243) — to_char/to_number number-picture sweep, 30 cases
//! against live PG18.4 (2026-07-19). RN/TH/EEEE/PR/MI/FM/L/D/G/V and the
//! to_number family already matched; the gaps:
//!
//!   * characters with no meaning in a number picture print AS
//!     THEMSELVES in PG (`XYZ999` → `XYZ 123`, `999XYZ` → ` 123XYZ`);
//!     SPG dropped them — the round-221 recorded residual;
//!   * a LEADING `PL` is its own column, `+` for non-negative and a
//!     space otherwise (`PL9999.9` → `+ 1234.5` / ` -1234.5`);
//!   * a value wider than a `V` picture's slots overflows to `#` per
//!     slot with literals kept (`to_char(12,'V99 999')` → ` ## ###`);
//!     SPG printed the scaled number full-width;
//!   * an interval literal accepts the number and unit RUN TOGETHER
//!     (`'15h 2m 12s'`, `'2d 3h'`, `'1.5h'`, `'-3h'`) — SPG's pair
//!     tokenizer required whitespace between them.
//!
//! Every expectation is byte-exact against PG (bracket-probed for the
//! space columns).

use spg_engine::{Engine, QueryResult};

fn one(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap_or_else(|x| panic!("{sql}: {x:?}")) {
        QueryResult::Rows { rows, .. } => spg_engine::eval::value_to_text(&rows[0].values[0]),
        other => panic!("{sql}: {other:?}"),
    }
}

#[test]
fn picture_literals_print_as_themselves() {
    let mut e = Engine::new();
    assert_eq!(one(&mut e, "SELECT to_char(123, 'XYZ999')"), "XYZ 123");
    assert_eq!(one(&mut e, "SELECT to_char(-123, 'XYZ999')"), "XYZ-123");
    assert_eq!(one(&mut e, "SELECT to_char(123, '999XYZ')"), " 123XYZ");
}

#[test]
fn leading_pl_is_its_own_column() {
    let mut e = Engine::new();
    assert_eq!(one(&mut e, "SELECT to_char(1234.5, 'PL9999.9')"), "+ 1234.5");
    assert_eq!(one(&mut e, "SELECT to_char(-1234.5, 'PL9999.9')"), " -1234.5");
    // Both ends at once.
    assert_eq!(one(&mut e, "SELECT to_char(1234.5, 'PL9999.9PL')"), "+ 1234.5+");
}

#[test]
fn v_scale_overflows_to_hashes_keeping_literals() {
    let mut e = Engine::new();
    assert_eq!(one(&mut e, "SELECT to_char(12, 'V99 999')"), " ## ###");
    // In-range V scaling is unchanged.
    assert_eq!(one(&mut e, "SELECT to_char(12.4, '99V99')"), " 1240");
    // The plain-path overflow was already right; guard it.
    assert_eq!(one(&mut e, "SELECT to_char(99999, '999')"), " ###");
}

#[test]
fn interval_units_may_run_together() {
    let mut e = Engine::new();
    assert_eq!(
        one(&mut e, "SELECT to_char(interval '15h 2m 12s', 'HH24:MI:SS')"),
        "15:02:12"
    );
    assert_eq!(
        one(&mut e, "SELECT extract(epoch from interval '2d 3h')::text"),
        "183600.000000"
    );
    assert_eq!(
        one(&mut e, "SELECT extract(epoch from interval '1.5h')::text"),
        "5400.000000"
    );
    assert_eq!(
        one(&mut e, "SELECT extract(epoch from interval '-3h')::text"),
        "-10800.000000"
    );
    // The spaced pair form is untouched.
    assert_eq!(
        one(&mut e, "SELECT extract(epoch from interval '2 hours 30 minutes')::text"),
        "9000.000000"
    );
}

#[test]
fn the_template_core_is_unchanged() {
    let mut e = Engine::new();
    // Regression guard over the sweep's clean cases.
    for (sql, want) in [
        ("SELECT to_char(1234.567, '9,999.99')", " 1,234.57"),
        ("SELECT to_char(-1234.5, 'S9999.9')", "-1234.5"),
        ("SELECT to_char(-1234.5, '9999.9MI')", "1234.5-"),
        ("SELECT to_char(-12, '99PR')", "<12>"),
        ("SELECT to_char(485, 'RN')", "        CDLXXXV"),
        ("SELECT to_char(0.5, 'FM9.99')", ".5"),
        ("SELECT to_char(0.0004859, '9.99EEEE')", " 4.86e-04"),
        ("SELECT to_number('1,234.56', '9,999.99')::text", "1234.56"),
    ] {
        assert_eq!(one(&mut e, sql), want, "{sql}");
    }
}
