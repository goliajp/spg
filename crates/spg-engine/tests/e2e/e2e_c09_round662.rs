//! v7.39 (round 662) — C09, and the record was wrong about half of it.
//!
//! C09 was opened in round 654 as "about twenty overloads PG has that SPG
//! refuses", derived from a probe that called every missing PG signature.
//! Re-measured against PG18 on the current tree, most of the list does not
//! survive:
//!
//!   * three-argument `lag`/`lead` ALREADY WORK. The round-654 probe called
//!     them as plain functions — `lag(1,1,1)` with no OVER — which fails for
//!     an unrelated reason and was read as a missing overload.
//!   * `date_part('day', time)` already matches PG, refusal and wording both.
//!   * two-argument `ts_rewrite`: both engines refuse.
//!   * ten "gaps" were the probe passing `'SELECT'` where a relation or
//!     function name goes, and six were `'a'` as range bound flags.
//!
//! What was real, and is fixed here, is two things — one of which C09 never
//! recorded:
//!
//!   * `'2020-01-01+00'::timestamptz` — a date carrying the zone with no
//!     time between them. The literal parser split on a space or a `T`, found
//!     neither, and handed the whole string to the date parser.
//!   * `to_char(real, …)` was refused outright, and chasing it turned up the
//!     cause: `real::numeric` rendered with Rust's shortest round-trip where
//!     PG uses six significant digits (`FLT_DIG`). SPG said `1.2345679` and
//!     `123456790` for values a float4 does not carry that precisely.

use spg_engine::{Engine, QueryResult};

fn one(e: &mut Engine, sql: &str) -> String {
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
            .collect::<Vec<_>>()
            .join(","),
        other => panic!("{sql}: {other:?}"),
    }
}

/// A date may carry the zone directly. The INSTANT is PG18's, verified over
/// the wire; the strings below are the embedded renderer's, which prints a
/// timestamptz without the `+00` the wire encoder appends. Recorded, not
/// fixed here: it is a difference between SPG's own two surfaces, and the
/// wire is the one PG compatibility is measured on.
#[test]
fn round662_a_date_can_carry_its_zone() {
    let mut e = Engine::new();
    assert_eq!(
        one(&mut e, "SELECT '2020-01-01+00'::timestamptz"),
        "2020-01-01 00:00:00"
    );
    assert_eq!(
        one(&mut e, "SELECT '2020-01-01Z'::timestamptz"),
        "2020-01-01 00:00:00"
    );
    assert_eq!(
        one(&mut e, "SELECT '2020-01-01+05:30'::timestamptz"),
        "2019-12-31 18:30:00"
    );
    // Unchanged shapes.
    assert_eq!(
        one(&mut e, "SELECT '2020-01-01 00:00:00+00'::timestamptz"),
        "2020-01-01 00:00:00"
    );
    assert_eq!(
        one(&mut e, "SELECT '2020-01-01'::timestamptz"),
        "2020-01-01 00:00:00"
    );
}

/// PG REFUSES a negative offset in this position — a trailing `-05` cannot be
/// told from the date's own hyphens, and it would rather reject than guess.
/// The first version here accepted it and answered an instant PG declines to
/// name; the differential caught it.
#[test]
fn round662_a_negative_offset_after_a_bare_date_is_refused() {
    let mut e = Engine::new();
    assert!(e.execute("SELECT '2020-01-01-05'::timestamptz").is_err());
}

/// `real::numeric` is six significant digits, `real::text` is the shortest
/// round-trip. Two different rules on the same value, both PG's.
#[test]
fn round662_real_narrows_to_six_significant_digits_as_numeric() {
    let mut e = Engine::new();
    assert_eq!(one(&mut e, "SELECT 12345.678::real::numeric"), "12345.7");
    assert_eq!(one(&mut e, "SELECT 1.23456789::real::numeric"), "1.23457");
    assert_eq!(one(&mut e, "SELECT 123456789::real::numeric"), "123457000");
    // …and the text rule is untouched.
    assert_eq!(one(&mut e, "SELECT 12345.678::real::text"), "12345.678");
    assert_eq!(one(&mut e, "SELECT 1.23456789::real::text"), "1.2345679");
}

/// `to_char(real, …)` exists now, and takes PG's route — through numeric,
/// not through float8. The two disagree: via float8 the same value formats
/// as `12345.68`.
#[test]
fn round662_to_char_accepts_real_and_routes_through_numeric() {
    let mut e = Engine::new();
    assert_eq!(one(&mut e, "SELECT to_char(1.5::real, '9.9')"), " 1.5");
    assert_eq!(one(&mut e, "SELECT to_char(2.5::float4, '99.99')"), "  2.50");
    assert_eq!(
        one(&mut e, "SELECT to_char(12345.678::real, 'FM999999.99')"),
        "12345.7"
    );
    assert_eq!(one(&mut e, "SELECT to_char(-3.25::real, '9.99')"), "-3.25");
    // float8 keeps its own route.
    assert_eq!(one(&mut e, "SELECT to_char(1.5::float8, '9.9')"), " 1.5");
}

/// Recorded in C09 as missing, measured as already present. Kept so the
/// claim cannot drift back into the ledger.
#[test]
fn round662_the_window_default_argument_was_never_missing() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE w(x int)").unwrap();
    e.execute("INSERT INTO w VALUES (1), (2)").unwrap();
    assert_eq!(
        one(&mut e, "SELECT lag(x, 1, -1) OVER (ORDER BY x) FROM w"),
        "-1,1"
    );
    assert_eq!(
        one(&mut e, "SELECT lead(x, 1, -9) OVER (ORDER BY x) FROM w"),
        "2,-9"
    );
}
