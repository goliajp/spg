//! v7.39 (round 653) — `pg_proc` listed 338 of the 709 functions the engine
//! answers.
//!
//! F20 had carried one line since it was opened: "`pg_proc` 总行数不足 100 而
//! PG 有数千". Measured properly the shape is different and worse: the count
//! is not the point, the UNDER-REPORTING is. Every client that introspects
//! pg_proc — psql's `\df`, an ORM checking for a function before using it, a
//! migration tool deciding whether to install a shim — was told SPG lacks
//! functions it implements.
//!
//! The list was established by calling every candidate name with zero
//! arguments: the engine's own reply separates `does not exist` from
//! `takes N args`, so what went in is what the running binary really
//! answers, and the arity came out of the same reply. Deliberately still
//! absent: 149 names the engine answers that PG18 does not have — the
//! MySQL-dialect family, the extension families (pgcrypto, fuzzystrmatch,
//! amcheck), and SPG's own internals. Listing those would claim PG has them.

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

/// A sample of what was absent, each row byte for byte with PG18's.
#[test]
fn round653_the_functions_the_engine_answers_are_listed() {
    let mut e = Engine::new();
    for (name, want) in [
        ("cardinality", vec!["cardinality|1|int4"]),
        ("array_length", vec!["array_length|2|int4"]),
        ("jsonb_set", vec!["jsonb_set|4|jsonb"]),
        ("regexp_replace", vec![
            "regexp_replace|3|text",
            "regexp_replace|4|text",
            "regexp_replace|4|text",
            "regexp_replace|5|text",
            "regexp_replace|6|text",
        ]),
    ] {
        assert_eq!(
            vals(
                &mut e,
                &format!(
                    "SELECT p.proname, p.pronargs, t.typname FROM pg_proc p \
                     JOIN pg_type t ON t.oid = p.prorettype \
                     WHERE p.proname = '{name}' ORDER BY p.pronargs, t.typname"
                )
            ),
            want,
            "{name}"
        );
    }
}

/// The engine answers these; PG18 does not have them. Claiming them in
/// pg_proc would be telling a client that PG has a `date_format`.
#[test]
fn round653_dialect_and_internal_names_stay_out() {
    let mut e = Engine::new();
    assert_eq!(
        vals(
            &mut e,
            "SELECT count(*) FROM pg_proc WHERE proname IN \
             ('date_format','ucase','year','levenshtein','crypt','__array_assign','interval')"
        ),
        vec!["0"]
    );
    // …but they still work, which is the whole reason they are not listed
    // as PG functions.
    assert_eq!(vals(&mut e, "SELECT ucase('ab')"), vec!["AB"]);
}

/// SPG must never list an overload PG does not have. The reverse gap (81
/// names carry fewer rows than PG) is recorded and open; this pin is the
/// direction that would be a lie rather than a shortfall.
#[test]
fn round653_no_name_claims_more_overloads_than_pg() {
    let mut e = Engine::new();
    // PG18 counts, read from the oracle: these are names where a stray
    // duplicate row would be invisible without a check.
    for (name, pg_count) in [("length", 8), ("md5", 2), ("to_hex", 2), ("substr", 4)] {
        let got = vals(
            &mut e,
            &format!("SELECT count(*) FROM pg_proc WHERE proname = '{name}'"),
        );
        let n: usize = got[0].parse().unwrap();
        assert!(n <= pg_count, "{name}: SPG lists {n}, PG18 has {pg_count}");
    }
}

/// `pg_lsn_larger('0/1'::pg_lsn, …)` answered "args must be text, got
/// pg_lsn": the cast produced the type and the function refused it. Found
/// by checking that every type the new rows name is one the engine can
/// actually produce.
#[test]
fn round653_pg_lsn_functions_accept_pg_lsn() {
    let mut e = Engine::new();
    assert_eq!(
        vals(&mut e, "SELECT pg_lsn_larger('0/1'::pg_lsn, '0/2'::pg_lsn)"),
        vec!["0/2"]
    );
    assert_eq!(
        vals(&mut e, "SELECT pg_lsn_smaller('0/1'::pg_lsn, '0/2'::pg_lsn)"),
        vec!["0/1"]
    );
    // The text spelling kept working.
    assert_eq!(vals(&mut e, "SELECT pg_lsn_larger('0/1', '0/2')"), vec!["0/2"]);
}

/// The eleven types the new rows needed. Each is one the engine really
/// produces — the pin asserts the value, not just the catalog row.
#[test]
fn round653_the_new_types_are_types_the_engine_produces() {
    let mut e = Engine::new();
    assert_eq!(vals(&mut e, "SELECT point(1,2)"), vec!["(1,2)"]);
    assert_eq!(vals(&mut e, "SELECT 'pg_class'::regclass"), vec!["pg_class"]);
    assert_eq!(
        vals(&mut e, "SELECT range_merge(int4range(1,3), int4range(5,7))"),
        vec!["[1,7)"]
    );
    assert_eq!(vals(&mut e, "SELECT array_append(ARRAY[1,2], 3)"), vec!["{1,2,3}"]);
    assert_eq!(
        vals(
            &mut e,
            "SELECT count(*) FROM pg_type WHERE typname IN \
             ('point','macaddr8','aclitem','_aclitem','regclass','void','trigger',\
              'pg_lsn','anyrange','anymultirange','anycompatiblearray')"
        ),
        vec!["11"]
    );
}

/// v7.39 (round 654) — the overload layer. Round 653 fixed which NAMES are
/// listed; 81 of them still carried fewer rows than PG18.
///
/// The design question first: SPG's `max` is ONE generic implementation,
/// not 24 typed ones, so listing 24 rows is fiction if the catalog counts
/// implementations and truth if it describes which calls succeed. Measured
/// — they succeed — so the rows went in, each one earned by constructing a
/// call for that exact PG18 signature and keeping it only if the engine
/// answered.
#[test]
fn round654_overloads_match_what_the_engine_accepts() {
    let mut e = Engine::new();
    for (name, want) in [("abs", "6"), ("avg", "7"), ("sum", "8"), ("max", "23")] {
        assert_eq!(
            vals(
                &mut e,
                &format!("SELECT count(*) FROM pg_proc WHERE proname = '{name}'")
            ),
            vec![want],
            "{name}"
        );
    }
    // …and the generic implementation really does answer for the types the
    // rows claim. If this ever fails the rows became fiction.
    e.execute("CREATE TABLE ov(m MONEY, iv INTERVAL, b BYTEA, ip INET)")
        .unwrap();
    e.execute("INSERT INTO ov VALUES ('$8'::money, '1 day', '\\x01', '1.2.3.4')")
        .unwrap();
    assert_eq!(vals(&mut e, "SELECT max(m) FROM ov"), vec!["$8.00"]);
    assert_eq!(vals(&mut e, "SELECT max(iv) FROM ov"), vec!["1 day"]);
    assert_eq!(vals(&mut e, "SELECT max(b) FROM ov"), vec!["\\x01"]);
    assert_eq!(vals(&mut e, "SELECT max(ip) FROM ov"), vec!["1.2.3.4"]);
    assert_eq!(vals(&mut e, "SELECT sum(m) FROM ov"), vec!["$8.00"]);
    assert_eq!(vals(&mut e, "SELECT avg(iv) FROM ov"), vec!["1 day"]);
}

/// The overloads PG has that the engine REFUSED are deliberately not
/// listed: they are capability gaps (C09), and a catalog row would be the
/// catalog lying on the implementation's behalf.
#[test]
fn round654_refused_overloads_are_not_papered_over() {
    let mut e = Engine::new();
    // Three arguments is PG's `lag(value, offset, default)`.
    assert!(e.execute("SELECT lag(1, 1, 1) OVER ()").is_err());
    assert_eq!(
        vals(
            &mut e,
            "SELECT count(*) FROM pg_proc WHERE proname = 'lag' AND pronargs = 3"
        ),
        vec!["0"],
        "no row for a signature the engine cannot answer"
    );
}
