//! v7.39 (round 634) — the rest of the registered casts, 9 down to 0.
//!
//! Round 633 probed PG's 129 registered casts between SPG's 33 base types
//! and found 23 SPG could not perform; it fixed the smallint widenings and
//! `timestamp -> time` and left nine, deliberately holding `pg_cast` back
//! rather than publish a catalog claiming conversions the engine refuses.
//! These are the nine, each measured against PG18 for its edge behaviour
//! before being written:
//!
//!     'ab'::CHAR(4)::"char"     a        first byte of the padded value
//!     'ab'::CHAR(4)::XML        ab       the text form, padding dropped
//!     '\x3132'::BYTEA::SMALLINT 12594    big-endian, ALL the bytes
//!     '\x31'::BYTEA::SMALLINT   49       one byte is just that byte
//!     ''::BYTEA::SMALLINT       0
//!     '\x313233'::BYTEA::SMALLINT        "smallint out of range"
//!     1::INT::REGPROC           1        an oid with no entry renders as itself
//!     TIME '01:02:03'::TIMETZ   01:02:03+00
//!     TIMESTAMPTZ '...'::TIMETZ 03:04:05+00
//!
//! The bpchar pair is the same shape as round 633's smallint one: the Text
//! arms were already there and a bpchar simply never reached them, because
//! the cast path does not normalise it the way the function dispatch does.
//!
//! With these, every cast PG performs between SPG's base types is one SPG
//! performs — 119 of 119 with the probe's values, the other ten being
//! value-domain failures on both engines rather than missing support.
//!
//! Recorded while pinning this: `'random'::REGPROC` resolves here and is
//! ambiguous in PG, whose `random` has overloads SPG's static proc table
//! does not carry. `abs`, `now` and an unknown name all agree.

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

fn err(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql) {
        Err(err) => err.to_string(),
        Ok(ok) => panic!("{sql}: expected a rejection, got {ok:?}"),
    }
}

#[test]
fn round634_bpchar_reaches_the_text_targets() {
    let mut e = Engine::new();
    assert_eq!(vals(&mut e, "SELECT 'ab'::CHAR(4)::\"char\""), vec!["a"]);
    assert_eq!(
        vals(&mut e, "SELECT 'ab'::CHAR(4)::XML"),
        vec!["ab"],
        "the padding goes, as it does through ::TEXT"
    );
    // Ill-formed content is still refused, through bpchar as through text.
    assert!(err(&mut e, "SELECT '<a>'::CHAR(8)::XML").contains("invalid XML"));
}

#[test]
fn round634_bytea_reads_big_endian() {
    let mut e = Engine::new();
    assert_eq!(
        vals(&mut e, "SELECT '\\x3132'::BYTEA::SMALLINT, '\\x31'::BYTEA::SMALLINT"),
        vec!["12594|49"]
    );
    assert_eq!(vals(&mut e, "SELECT ''::BYTEA::SMALLINT"), vec!["0"]);
    assert!(
        err(&mut e, "SELECT '\\x313233'::BYTEA::SMALLINT").contains("out of range"),
        "three bytes do not fit a smallint, and PG says so too"
    );
    // The wider targets take what fits.
    assert_eq!(
        vals(&mut e, "SELECT '\\x313233'::BYTEA::INT, '\\x3132'::BYTEA::BIGINT"),
        vec!["3224115|12594"]
    );
}

#[test]
fn round634_an_oid_reaches_regproc() {
    let mut e = Engine::new();
    assert_eq!(
        vals(&mut e, "SELECT 1::INT::REGPROC, 1::SMALLINT::REGPROC, 1::BIGINT::REGPROC"),
        vec!["1|1|1"],
        "an oid with no matching entry renders as the number, as in PG"
    );
    // The name form still resolves, and still refuses an unknown name.
    // `now` has one entry; `abs` has several and both engines say so.
    assert_eq!(vals(&mut e, "SELECT 'now'::REGPROC"), vec!["now"]);
    assert!(err(&mut e, "SELECT 'abs'::REGPROC").contains("more than one function"));
    assert!(err(&mut e, "SELECT 'nosuchfn'::REGPROC").contains("does not exist"));
    // v7.39 (round 654) — this WAS the recorded divergence: "'random'::REGPROC
    // resolves here and is ambiguous in PG, which has overloads of it that
    // SPG's static table does not carry". Filling in the overloads closed it
    // without anyone aiming at it — both engines now answer
    // `more than one function named "random"`, byte for byte.
    assert!(err(&mut e, "SELECT 'random'::REGPROC").contains("more than one function"));
}

#[test]
fn round634_time_and_timestamp_reach_timetz() {
    let mut e = Engine::new();
    assert_eq!(vals(&mut e, "SELECT TIME '01:02:03'::TIMETZ"), vec!["01:02:03+00"]);
    assert_eq!(
        vals(&mut e, "SELECT TIMESTAMPTZ '2020-01-02 03:04:05+00'::TIMETZ"),
        vec!["03:04:05+00"]
    );
    assert_eq!(
        vals(&mut e, "SELECT TIMESTAMP '1969-12-31 23:00:00'::TIMETZ"),
        vec!["23:00:00+00"],
        "before the epoch, where a plain remainder would go negative"
    );
    // The text entry point is unchanged.
    assert_eq!(vals(&mut e, "SELECT '01:02:03+00'::TIMETZ"), vec!["01:02:03+00"]);
}
