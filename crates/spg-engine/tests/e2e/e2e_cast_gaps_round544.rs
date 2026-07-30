//! v7.39 (round 544) — the conversions PG performs and SPG refused.
//!
//! pg_dump's next stop was pg_cast, so this round tried to publish one
//! by PROBING the real cast function — one sample value per type, then
//! every ordered pair asked of `cast_value` — rather than writing a
//! table beside the implementation for it to drift from.
//!
//! The probe was thrown away and the round kept what it found.
//!
//! PG's pg_cast is a REGISTRY, not a description of what converts: it
//! lists `bool → text` and does not list `int4 → text`, which PG
//! resolves through the type's I/O functions with no row at all. So its
//! content cannot be derived from behaviour. Measured over the
//! thirty-seven types SPG catalogues, the probe reported 180 pairs PG
//! does not list and missed 31 it does — and some of those 31 only
//! because the sample chosen for a type could not convert, which makes
//! a WORKING cast look absent (`jsonb → numeric` reads fine on SPG; the
//! sample was `{}`). A catalog built on a heuristic is not a catalog,
//! so pg_cast ships empty, which is the accurate answer to the question
//! it asks: SPG registers no casts and has no CREATE CAST.
//!
//! What the comparison found is here. Five families PG converts and SPG
//! answered with an error:
//!
//!     '10:20:30+09'::timetz::time    10:20:30            (zone dropped)
//!     '25:00:00'::interval::time     01:00:00            (modulo a day)
//!     '-1 hour'::interval::time      23:00:00            (…and wrapping)
//!     '01:02:03'::time::interval     01:02:03
//!     5::int4::bytea                 \x00000005
//!     '\xffffffff'::bytea::int4      -1
//!
//! Every expectation below is a PG18 reading.

use spg_engine::{Engine, QueryResult};

fn one(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err}")) {
        QueryResult::Rows { rows, .. } => rows
            .first()
            .map(|r| {
                r.values
                    .iter()
                    .map(spg_engine::eval::value_to_text)
                    .collect::<Vec<_>>()
                    .join("|")
            })
            .unwrap_or_default(),
        other => panic!("{sql}: {other:?}"),
    }
}

/// A time with a zone becomes the wall clock, the zone dropped.
#[test]
fn round544_timetz_to_time() {
    let mut e = Engine::new();
    assert_eq!(one(&mut e, "SELECT '10:20:30+09'::timetz::time"), "10:20:30");
    assert_eq!(
        one(&mut e, "SELECT '10:20:30.5+09'::timetz::time"),
        "10:20:30.5"
    );
}

/// An interval becomes a time modulo a day, negatives wrapping.
#[test]
fn round544_interval_to_time() {
    let mut e = Engine::new();
    for (expr, want) in [
        ("'01:02:03'::interval::time", "01:02:03"),
        // Past midnight wraps rather than saturating.
        ("'25:00:00'::interval::time", "01:00:00"),
        ("'-1 hour'::interval::time", "23:00:00"),
        // Days do not count toward the time of day.
        ("'1 day 02:00:00'::interval::time", "02:00:00"),
        ("'2 days'::interval::time", "00:00:00"),
    ] {
        assert_eq!(one(&mut e, &format!("SELECT {expr}")), want, "{expr}");
    }
}

/// A time of day IS an interval of that length, fractional seconds and all.
#[test]
fn round544_time_to_interval() {
    let mut e = Engine::new();
    assert_eq!(one(&mut e, "SELECT '01:02:03'::time::interval"), "01:02:03");
    assert_eq!(
        one(&mut e, "SELECT '10:20:30.123456'::time::interval"),
        "10:20:30.123456"
    );
}

/// An integer's two's-complement bytes, big-endian, at its own width.
#[test]
fn round544_integer_to_bytea() {
    let mut e = Engine::new();
    for (expr, want) in [
        ("5::int2::bytea", "\\x0005"),
        ("5::int4::bytea", "\\x00000005"),
        ("5::int8::bytea", "\\x0000000000000005"),
        ("(-1)::int4::bytea", "\\xffffffff"),
    ] {
        assert_eq!(one(&mut e, &format!("SELECT {expr}")), want, "{expr}");
    }
}

/// And back: big-endian, right-aligned, sign-extended at full width.
#[test]
fn round544_bytea_to_integer() {
    let mut e = Engine::new();
    for (expr, want) in [
        ("'\\x05'::bytea::int4", "5"),
        ("'\\x05'::bytea::int8", "5"),
        ("'\\x0000000000000005'::bytea::int8", "5"),
        // An empty bytea is zero, not an error.
        ("'\\x'::bytea::int4", "0"),
        // A full-width leading bit is a sign bit.
        ("'\\xffffffff'::bytea::int4", "-1"),
    ] {
        assert_eq!(one(&mut e, &format!("SELECT {expr}")), want, "{expr}");
    }
    // Wider than the target is refused rather than truncated.
    assert!(
        e.execute("SELECT '\\x0000000000000005'::bytea::int4").is_err(),
        "eight bytes must not fit an int4"
    );
}

/// pg_cast exists and lists nothing, which is what lets pg_dump past it.
#[test]
fn round544_pg_cast_is_an_empty_registry() {
    let mut e = Engine::new();
    // v7.39 (round 635) — it is not empty any more. The registry carries
    // the 129 casts PG registers between SPG's base types, each one probed
    // against the engine in rounds 633/634 before being published. The
    // column list below is what this test was really guarding.
    assert_eq!(one(&mut e, "SELECT count(*) FROM pg_cast"), "129");
    assert_eq!(
        match e.execute("SELECT * FROM pg_catalog.pg_cast").unwrap() {
            QueryResult::Rows { columns, .. } =>
                columns.iter().map(|c| c.name.clone()).collect::<Vec<_>>(),
            other => panic!("{other:?}"),
        },
        vec![
            "oid",
            "castsource",
            "casttarget",
            "castfunc",
            "castcontext",
            "castmethod",
        ]
    );
}

/// The conversions that already worked keep working — the probe claimed
/// `jsonb → numeric` was missing, and it never was.
#[test]
fn round544_the_probes_false_negatives_were_false() {
    let mut e = Engine::new();
    assert_eq!(
        one(&mut e, "SELECT ('{\"a\":1}'::jsonb->'a')::numeric"),
        "1"
    );
    assert_eq!(one(&mut e, "SELECT ('{\"a\":1}'::jsonb->'a')::int"), "1");
    assert_eq!(
        one(&mut e, "SELECT ('{\"a\":true}'::jsonb->'a')::bool"),
        "true"
    );
    assert_eq!(one(&mut e, "SELECT 'true'::bool::int"), "1");
}
