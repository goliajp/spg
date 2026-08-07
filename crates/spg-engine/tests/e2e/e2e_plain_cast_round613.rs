//! v7.39 (round 613) — a plain cast target stops walking the whole arm, and
//! `::CHARACTER VARYING` stops truncating to one character.
//!
//! Round 612 measured `s::VARCHAR` at 30.6 ms over 200k rows where the
//! identical `s::TEXT` cost 11.2, and four probes showed the difference was
//! not one hotspot but the whole `Named` arm being re-derived for every row:
//! a name fixed for the statement, compared against every arm above the
//! resolve, on every row. The plain scalar spellings now go straight to the
//! tail:
//!
//!     s::VARCHAR          30.6 -> 17.4 ms      (200k rows)
//!     id::NUMERIC         26.2 -> 12.2
//!     id::REAL            20.9 -> 12.2
//!     s::VARCHAR(20)      35.3 -> 25.8
//!     s::CHAR(20)         43.0 -> 36.5
//!     id::NUMERIC(20,6)   31.8 -> 22.2
//!
//! and over pgwire on 500k rows against PG18:
//!
//!     s::VARCHAR                    90.43 -> 45.82   PG 6.04  11.61x -> 7.59x
//!     s::VARCHAR(20)               104.35 -> 62.54   PG 7.72  11.46x -> 8.10x
//!     id::NUMERIC                   65.54 -> 32.64   PG 7.73   8.52x -> 4.22x
//!     id::REAL                      56.04 -> 28.58   PG 6.18
//!     (id::NUMERIC/7)::NUMERIC(20,6) 178.03 -> 124.59 PG 17.05
//!
//! Taking the shortcut is only the same thing as walking the arm while every
//! name in it is claimed by no arm above the resolve, and resolves to the
//! type the table gives it. Both are asserted mechanically, in
//! `eval::cast`'s own unit tests, over every entry and every typmod head —
//! not by eye, so a name that later grows a special case fails the gate.
//! This file is the behavioural half: the shortcut and the general path have
//! to answer the same thing.
//!
//! 🔴 The differential written for that turned up a silent wrong answer that
//! had nothing to do with the shortcut and predates it (verified against the
//! previous binary): the cast-target parser folds `bit varying` into
//! `varbit` but had no fold for `character varying`, so the `varying` was
//! left behind and the cast became a bare `character` — which is `char(1)`.
//!
//!     'ab'::CHARACTER VARYING      PG 'ab'      SPG 'a'
//!
//! for a spelling pg_dump writes. The symmetric fold is in now, and the two
//! spellings agree with PG.
//!
//! Recorded, not fixed: `pg_typeof` reports `text` for every varchar
//! spelling where PG reports `character varying` — including plain
//! `::VARCHAR`, so it is the varchar type's own gap rather than this fold's.

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

/// The defect the differential found: two words, one type.
#[test]
fn round613_character_varying_is_varchar() {
    let mut e = Engine::new();
    assert_eq!(
        vals(
            &mut e,
            "SELECT 'ab'::CHARACTER VARYING, 'ab'::character varying"
        ),
        vec!["ab|ab"],
        "not 'a' — the fold was missing and it became a bare `character`"
    );
    assert_eq!(
        vals(&mut e, "SELECT 'abcdef'::CHARACTER VARYING(3)"),
        vec!["abc"],
        "and the typmod still reaches the resolver"
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT 'ab'::CHARACTER, 'ab'::CHARACTER(4)||'|', 'ab'::char||'|'"
        ),
        vec!["a|ab||a|"],
        "a bare CHARACTER is still char(1), which is what made the bug silent"
    );
}

/// Every spelling that takes the shortcut.
#[test]
fn round613_plain_spellings() {
    let mut e = Engine::new();
    assert_eq!(
        vals(
            &mut e,
            "SELECT 'ab'::VARCHAR, 'ab'::varchar, 'ab'::TEXT, 'ab'::text"
        ),
        vec!["ab|ab|ab|ab"]
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT 42::NUMERIC, 42::decimal, 42::REAL, 42::float4, 42::FLOAT8, 42::double precision"
        ),
        vec!["42|42|42|42|42|42"]
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT 42::int2, 42::SMALLINT, 42::int4, 42::INTEGER, 42::int8"
        ),
        vec!["42|42|42|42|42"]
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT 't'::bool, 't'::BOOLEAN, '2020-01-02'::date, \
             '00000000-0000-0000-0000-000000000001'::uuid"
        ),
        vec!["true|true|2020-01-02|00000000-0000-0000-0000-000000000001"]
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT 42::VARCHAR, 1.50::VARCHAR, true::VARCHAR, '2020-01-02'::DATE::VARCHAR"
        ),
        vec!["42|1.50|true|2020-01-02"],
        "a non-text source is stringified on the way in"
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT NULL::VARCHAR IS NULL, NULL::NUMERIC IS NULL, NULL::date IS NULL"
        ),
        vec!["true|true|true"]
    );
}

/// The typmod spellings, which resolve through the same type table.
#[test]
fn round613_typmod_spellings() {
    let mut e = Engine::new();
    assert_eq!(
        vals(
            &mut e,
            "SELECT 'abcdef'::VARCHAR(3), 'ab'::CHAR(4)||'|', 'ab'::bpchar||'|'"
        ),
        vec!["abc|ab||ab|"]
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT 1.23456::NUMERIC(10,2), 1.23456::decimal(10,2), 1.23456::NUMERIC(10, 2)"
        ),
        vec!["1.23|1.23|1.23"],
        "including a space after the comma"
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT '日本語'::VARCHAR(2), '日本語'::CHAR(2), 'ábç'::VARCHAR(2)"
        ),
        vec!["日本|日本|áb"],
        "the length cap counts characters"
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT ''::VARCHAR(3), ''::CHAR(3)||'|', 'abc'::VARCHAR(3), 'abc'::CHAR(3)||'|'"
        ),
        vec!["|||abc|abc|"]
    );
    assert!(
        e.execute("SELECT 1::numeric(1000,999)").is_err(),
        "a typmod outside PG's range is still refused"
    );
}

/// The names that must NOT take the shortcut still reach their own arm.
#[test]
fn round613_names_with_their_own_arm() {
    let mut e = Engine::new();
    assert_eq!(
        vals(&mut e, "SELECT 5::BIT(4), 1::BIT"),
        vec!["0101|1"],
        "the bit widths are read before anything else"
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT '2020-01-02 03:04:05.678'::TIMESTAMP(0), '03:04:05.678'::TIME(1)"
        ),
        vec!["2020-01-02 03:04:06|03:04:05.7"],
        "a temporal precision still rounds"
    );
    assert_eq!(
        vals(&mut e, "SELECT NULL::anyarray IS NULL, '{1,2}'::INT[]"),
        vec!["true|{1,2}"]
    );
    assert!(e.execute("SELECT 'abc'::regproc").is_err());
    assert!(e.execute("SELECT 1::nosuchtype").is_err());
    assert_eq!(
        vals(
            &mut e,
            "SELECT 'Infinity'::NUMERIC, 'NaN'::numeric, 1e10::NUMERIC, 0.1::real::numeric"
        ),
        vec!["Infinity|NaN|10000000000|0.1"],
        "the NUMERIC specials are unchanged by the shortcut"
    );
}

/// At the size where walking the arm per row was the cost.
#[test]
fn round613_scale() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE big (id INT, s TEXT)").unwrap();
    e.execute("INSERT INTO big SELECT gg, 'row' || gg FROM generate_series(1, 20000) gg")
        .unwrap();
    assert_eq!(
        vals(
            &mut e,
            "SELECT count(s::VARCHAR), count(s::VARCHAR(4)), count(s::TEXT) FROM big"
        ),
        vec!["20000|20000|20000"]
    );
    assert_eq!(
        vals(&mut e, "SELECT count(DISTINCT s::VARCHAR(4)) FROM big"),
        vals(&mut e, "SELECT count(DISTINCT left(s, 4)) FROM big"),
        "the length cap cuts where `left` does"
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT sum(id::NUMERIC), sum(id::NUMERIC(10,2)) FROM big"
        ),
        vec!["200010000|200010000.00"],
        "checked against live PG18, which answers the same"
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT count(*) FROM big WHERE s::CHARACTER VARYING = s"
        ),
        vec!["20000"],
        "the folded spelling is the same value the column already holds"
    );
}
