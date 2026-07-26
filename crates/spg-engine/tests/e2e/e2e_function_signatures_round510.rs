//! v7.39 (round 510) — signatures of functions SPG already had.
//!
//! The `pg_proc` sweep turned up a class distinct from "missing function":
//! the NAME resolves and one arity works, so nothing looked wrong, while the
//! other spellings PG defines were arity errors. `random(1, 10)` — the form
//! an application actually reaches for — answered "random() takes 0 args,
//! got 2".
//!
//! `ts_rank` is the odd one and worth reading twice. Every form of it worked
//! with real values; only an all-NULL call failed, because the optional
//! weight array and norm flag are sorted out by their VALUE shape and a NULL
//! matches neither, so the argument list looked malformed. PG is strict
//! there and answers NULL. The sweep saw it because every probe it writes is
//! a NULL call — which is also how it saw round 509's cast bug.
//!
//! Every expectation below is a PG18 reading.

use spg_engine::{Engine, QueryResult};

fn engine() -> Engine {
    Engine::new()
}

fn text(e: &mut Engine, sql: &str) -> String {
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

/// PG18's `random(min, max)` draws from a CLOSED range and answers in the
/// operands' own type.
#[test]
fn round510_random_takes_a_range() {
    let mut e = engine();
    // A degenerate range pins both the value and the inclusivity.
    assert_eq!(text(&mut e, "SELECT random(1,1), random(5,5), random(2.5,2.5)"), "1|5|2.5");
    assert_eq!(
        text(
            &mut e,
            "SELECT pg_typeof(random(1,1)), pg_typeof(random(1::bigint,1::bigint)), \
             pg_typeof(random(1.5,1.5))"
        ),
        "integer|bigint|numeric"
    );
    // A real range stays inside it.
    for _ in 0..20 {
        assert_eq!(text(&mut e, "SELECT random(1,10) BETWEEN 1 AND 10"), "true");
    }
    // Both ends are reachable over enough draws — the closed range again,
    // from the other side.
    let mut saw_low = false;
    let mut saw_high = false;
    for _ in 0..200 {
        match text(&mut e, "SELECT random(1,3)").as_str() {
            "1" => saw_low = true,
            "3" => saw_high = true,
            "2" => {}
            other => panic!("random(1,3) answered {other}"),
        }
    }
    assert!(saw_low && saw_high, "both ends of the range must occur");
    assert_eq!(text(&mut e, "SELECT random(NULL::int, 3)"), "NULL");
    // Reversed bounds are PG's own error.
    let err = format!("{}", e.execute("SELECT random(10,1)").unwrap_err());
    assert!(
        err.contains("lower bound must be less than or equal to upper bound"),
        "got {err}"
    );
}

/// The flag string every other regexp function here already took.
#[test]
fn round510_regexp_split_to_array_takes_flags() {
    let mut e = engine();
    assert_eq!(
        text(&mut e, "SELECT regexp_split_to_array('aXbXc','x','i')"),
        "{a,b,c}"
    );
    // Without the flag the pattern is case-sensitive, so nothing splits.
    assert_eq!(
        text(&mut e, "SELECT regexp_split_to_array('aXbXc','x')"),
        "{aXbXc}"
    );
}

/// `length(bytea, encoding)` counts CHARACTERS, which is what the encoding
/// argument is for.
#[test]
fn round510_length_takes_an_encoding() {
    let mut e = engine();
    assert_eq!(text(&mut e, "SELECT length('abc'::bytea, 'UTF8')"), "3");
    assert_eq!(text(&mut e, "SELECT length(NULL::bytea, 'UTF8')"), "NULL");
    // Multi-byte: four bytes, two characters.
    assert_eq!(
        text(&mut e, "SELECT length('\\xc3a9c3a9'::bytea, 'UTF8'), length('\\xc3a9c3a9'::bytea)"),
        "2|4"
    );
}

/// `ts_rank` is strict: every form works with values, and a NULL anywhere
/// makes the answer NULL rather than an argument-list complaint.
#[test]
fn round510_ts_rank_is_strict() {
    let mut e = engine();
    for sql in [
        "SELECT ts_rank(NULL::tsvector, NULL::tsquery, NULL::integer)",
        "SELECT ts_rank(NULL::tsvector, NULL::tsquery)",
        "SELECT ts_rank_cd(NULL::tsvector, NULL::tsquery, NULL::integer)",
        "SELECT ts_rank('cat:1'::tsvector, NULL::tsquery)",
    ] {
        assert_eq!(text(&mut e, sql), "NULL", "{sql}");
    }
    // The forms that already worked still do, and still agree with each
    // other on a document that carries no weights.
    let plain = text(&mut e, "SELECT ts_rank('cat:1'::tsvector,'cat'::tsquery)");
    for sql in [
        "SELECT ts_rank('cat:1'::tsvector,'cat'::tsquery,0)",
        "SELECT ts_rank('{0.1,0.2,0.4,1.0}'::real[],'cat:1'::tsvector,'cat'::tsquery)",
        "SELECT ts_rank('{0.1,0.2,0.4,1.0}'::real[],'cat:1'::tsvector,'cat'::tsquery,0)",
    ] {
        assert_eq!(text(&mut e, sql), plain, "{sql}");
    }
}

/// `uuidv7(shift)` offsets the embedded timestamp, which is how a caller
/// mints an id ordered as of another moment.
#[test]
fn round510_uuidv7_takes_a_shift() {
    let mut e = engine();
    assert_eq!(text(&mut e, "SELECT uuidv7(INTERVAL '0') IS NOT NULL"), "true");
    assert_eq!(text(&mut e, "SELECT uuidv7(NULL::interval)"), "NULL");
    // The shift really moves the clock: a day back sorts before a day on.
    assert_eq!(
        text(
            &mut e,
            "SELECT uuidv7(INTERVAL '-1 day')::text < uuidv7(INTERVAL '1 day')::text"
        ),
        "true"
    );
}

/// The two-point spellings of the axis predicates.
#[test]
fn round510_two_point_axis_predicates() {
    let mut e = engine();
    assert_eq!(
        text(
            &mut e,
            "SELECT isvertical(point '(0,0)', point '(0,1)'), \
             ishorizontal(point '(0,0)', point '(1,0)'), \
             isvertical(point '(0,0)', point '(1,1)')"
        ),
        "true|true|false"
    );
    assert_eq!(
        text(&mut e, "SELECT isvertical(NULL::point, point '(0,1)')"),
        "NULL"
    );
    // The one-argument forms are untouched.
    assert_eq!(
        text(&mut e, "SELECT isvertical(lseg '((0,0),(0,1))')"),
        "true"
    );
}
