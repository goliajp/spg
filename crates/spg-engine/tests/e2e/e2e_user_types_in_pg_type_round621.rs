//! v7.39 (round 621) — a user-defined type existed everywhere except the
//! catalog that lists types.
//!
//! `CREATE TYPE mood AS ENUM (…)` then `SELECT typname FROM pg_type WHERE
//! typtype = 'e'` found nothing. So did the composite and the domain. The
//! types themselves worked — casting, comparing, `pg_typeof` — and `pg_enum`
//! listed the enum's three labels; only `pg_type` did not know they existed,
//! which is the one place a client looks to find out.
//!
//! `pg_enum` made it worse than an omission: its `enumtypid` counted from
//! 50_000 in catalog order, so the standard `pg_enum JOIN pg_type` joined
//! against nothing and came back empty. Two synthesised catalogs deriving the
//! same OIDs by iteration order is how they would drift apart again, so both
//! now read one function — the seventh time in rounds 620 and 621 that one
//! piece of knowledge had more than one home.
//!
//! `format_type` was the third reader and had to be told as well: it answered
//! `???` for a row `pg_type` had just started returning.
//!
//! The bands are disjoint by construction — enums from 50_001, composites from
//! 54_001, domains from 58_001, `pg_enum`'s per-label OIDs from 60_001 — and
//! `typbasetype` carries the domain's base OID, which is what tells a client
//! the domain is over an integer.
//!
//! All ten shapes were checked against live PG18 and match byte for byte.

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

fn seed() -> Engine {
    let mut e = Engine::new();
    e.execute("CREATE TYPE mood AS ENUM ('sad','ok','happy')").unwrap();
    e.execute("CREATE TYPE pt AS (x INT, y INT)").unwrap();
    e.execute("CREATE DOMAIN posint AS INT CHECK (VALUE > 0)").unwrap();
    e
}

/// The three kinds, and what `typtype` calls each.
#[test]
fn round621_user_types_are_in_pg_type() {
    let mut e = seed();
    assert_eq!(
        vals(
            &mut e,
            "SELECT typname, typtype FROM pg_type WHERE typname IN ('mood','pt','posint') ORDER BY 1"
        ),
        vec!["mood|e", "posint|d", "pt|c"]
    );
    assert_eq!(
        vals(&mut e, "SELECT typname, typcategory FROM pg_type WHERE typname = 'mood'"),
        vec!["mood|E"]
    );
    assert_eq!(
        vals(&mut e, "SELECT typname FROM pg_type WHERE typtype = 'e' ORDER BY 1"),
        vec!["mood"],
        "the query a client writes to enumerate enums"
    );
    assert_eq!(
        vals(&mut e, "SELECT typname FROM pg_type WHERE typtype = 'c' AND typname = 'pt'"),
        vec!["pt"]
    );
    assert_eq!(
        vals(&mut e, "SELECT typname FROM pg_type WHERE typtype = 'd' AND typname = 'posint'"),
        vec!["posint"]
    );
}

/// The join that came back empty.
#[test]
fn round621_pg_enum_joins_pg_type() {
    let mut e = seed();
    assert_eq!(
        vals(
            &mut e,
            "SELECT enumlabel FROM pg_enum e JOIN pg_type t ON e.enumtypid = t.oid \
             WHERE t.typname = 'mood' ORDER BY e.enumsortorder"
        ),
        vec!["sad", "ok", "happy"],
        "labels in declaration order, through the OID both catalogs now agree on"
    );
    assert_eq!(vals(&mut e, "SELECT count(*) FROM pg_enum"), vec!["3"]);
}

/// The third reader, and the domain's base.
#[test]
fn round621_format_type_and_typbasetype() {
    let mut e = seed();
    assert_eq!(
        vals(&mut e, "SELECT format_type(t.oid, -1) FROM pg_type t WHERE t.typname = 'mood'"),
        vec!["mood"],
        "answered `???` before — for a row pg_type had just started returning"
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT format_type(t.oid, -1) FROM pg_type t WHERE t.typname IN ('pt','posint') ORDER BY 1"
        ),
        vec!["posint", "pt"]
    );
    assert_eq!(
        vals(&mut e, "SELECT typbasetype FROM pg_type WHERE typname = 'posint'"),
        vec!["23"],
        "the domain names its base, which is how a client learns it is an integer"
    );
    assert_eq!(
        vals(&mut e, "SELECT typbasetype FROM pg_type WHERE typname = 'mood'"),
        vec!["0"],
        "an enum has no base"
    );
}

/// The types still work, and the builtins are untouched.
#[test]
fn round621_the_types_themselves_are_unchanged() {
    let mut e = seed();
    assert_eq!(vals(&mut e, "SELECT 'happy'::mood"), vec!["happy"]);
    assert_eq!(vals(&mut e, "SELECT pg_typeof('happy'::mood)"), vec!["mood"]);
    assert_eq!(
        vals(&mut e, "SELECT typname FROM pg_type WHERE oid = 23"),
        vec!["int4"],
        "a builtin still reads back as its internal typname"
    );
    assert_eq!(
        vals(&mut e, "SELECT count(*) > 50 FROM pg_type"),
        vec!["true"],
        "and the builtin rows are all still there"
    );
}
