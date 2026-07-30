//! v7.39 (round 633) — `1::SMALLINT::INT` said the cast did not exist.
//!
//! This round set out to synthesise `pg_cast`, whose SPG copy has no rows
//! against PG's 235. The construction was to be measurable rather than
//! declared: take PG's registered casts, restrict them to the types SPG
//! has, and emit only the pairs SPG can actually perform. Probing that
//! second step is what found the gaps, and they matter more than the
//! catalog does.
//!
//! Of the 129 registered casts between SPG's 33 base types, PG itself
//! performs 119 with the probe's values (the other ten are value-domain
//! failures — `'{}'::JSONB::INT` is not a cast problem). SPG could not
//! perform 23 of them, and the first two on the list are as ordinary as a
//! cast gets:
//!
//!     1::SMALLINT::INT      "cannot cast smallint to int"
//!     1::SMALLINT::BIGINT   "cannot cast smallint to bigint"
//!
//! Both arms were simply absent from the variant lists, next to the Int and
//! BigInt ones — the same shape as the sum accumulator missing SmallInt in
//! round 626. A hand-written variant list, one entry short.
//!
//!     TIMESTAMP '...'::TIME     "cannot cast timestamp ... to time ..."
//!
//! and the same for a timestamptz, which SPG carries in the same variant.
//! Taking the time of day out of a timestamp is an everyday operation and
//! PG registers it as an assignment cast.
//!
//! Recorded, measured, still missing: bpchar -> "char", bpchar -> xml,
//! bytea -> int2, the four integer -> regproc casts, and the `timetz`
//! targets — `::TIMETZ` reports "USER-DEFINED", which is a parser-level gap
//! rather than a missing conversion, so it belongs with its own item.

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

#[test]
fn round633_smallint_widens() {
    let mut e = Engine::new();
    assert_eq!(
        vals(&mut e, "SELECT 1::SMALLINT::INT, 1::SMALLINT::BIGINT"),
        vec!["1|1"]
    );
    assert_eq!(
        vals(&mut e, "SELECT (-32768)::SMALLINT::INT, 32767::SMALLINT::BIGINT"),
        vec!["-32768|32767"],
        "both ends of the range"
    );
    // Through a column, which is where it would have been met in practice.
    e.execute("CREATE TABLE w (n SMALLINT)").unwrap();
    e.execute("INSERT INTO w VALUES (7),(8)").unwrap();
    assert_eq!(vals(&mut e, "SELECT sum(n::BIGINT), max(n::INT) FROM w"), vec!["15|8"]);
    // The rest of the numeric matrix, which already worked.
    assert_eq!(
        vals(&mut e, "SELECT 1::SMALLINT::NUMERIC, 1::SMALLINT::REAL, 1::SMALLINT::TEXT"),
        vec!["1|1|1"]
    );
    assert_eq!(
        vals(&mut e, "SELECT 1::INT::SMALLINT, 1::BIGINT::SMALLINT, 1::INT::BIGINT"),
        vec!["1|1|1"]
    );
}

#[test]
fn round633_timestamp_yields_its_time_of_day() {
    let mut e = Engine::new();
    assert_eq!(
        vals(&mut e, "SELECT TIMESTAMP '2020-01-02 03:04:05'::TIME"),
        vec!["03:04:05"]
    );
    assert_eq!(
        vals(&mut e, "SELECT TIMESTAMPTZ '2020-01-02 03:04:05+00'::TIME"),
        vec!["03:04:05"]
    );
    assert_eq!(
        vals(&mut e, "SELECT TIMESTAMP '2020-01-02 00:00:00'::TIME"),
        vec!["00:00:00"],
        "midnight, where a plain remainder would still be zero"
    );
    // Before the epoch, where `%` would give a negative time.
    assert_eq!(
        vals(&mut e, "SELECT TIMESTAMP '1969-12-31 23:00:00'::TIME"),
        vec!["23:00:00"]
    );
    // The date half still works the way it did.
    assert_eq!(
        vals(&mut e, "SELECT TIMESTAMP '2020-01-02 03:04:05'::DATE"),
        vec!["2020-01-02"]
    );
}
