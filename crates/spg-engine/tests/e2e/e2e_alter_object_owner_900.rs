//! 9.0.0 — `ALTER … OWNER TO` recorded the owner for a TABLE and for
//! nothing else.
//!
//! Measured against PostgreSQL 18.6 over all six spellings: PG records
//! the new owner for a table, a sequence, a view, a materialized view, a
//! type (enum / composite / domain alike) and a function. SPG recorded
//! it for the TABLE alone — the sequence and domain forms validated that
//! the role existed and then did nothing, and the view / materialized
//! view / function forms fell into the parser's consume-to-boundary
//! tail, which reported success and read nothing at all.
//!
//! A dump therefore re-created every one of them owned by whoever ran
//! the restore.
//!
//! The materialized-view owner is the BACKING TABLE's `schema.owner`,
//! which is the one store both `pg_class.relowner` and
//! `pg_matviews.matviewowner` read. Writing an `object_owners` entry for
//! it instead — which the first cut did — is a second store that
//! nothing reads, and `relowner` went on answering the old owner.

use spg_engine::{Engine, QueryResult};

fn col(e: &mut Engine, sql: &str) -> Vec<String> {
    let QueryResult::Rows { rows, .. } = e
        .execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"))
    else {
        panic!("expected rows for {sql}");
    };
    rows.into_iter()
        .map(|r| spg_engine::eval::value_to_text(r.values.first().expect("one column")))
        .collect()
}

fn err(e: &mut Engine, sql: &str) -> String {
    format!("{}", e.execute(sql).unwrap_err())
}

fn seeded() -> Engine {
    let mut e = Engine::new();
    for sql in [
        "CREATE USER o9 WITH PASSWORD 'x'",
        "CREATE TABLE ot (id int)",
        "CREATE SEQUENCE os",
        "CREATE VIEW ov AS SELECT 1 AS a",
        "CREATE MATERIALIZED VIEW omv AS SELECT 1 AS a",
        "CREATE TYPE oc AS (a int)",
        "CREATE DOMAIN od AS int",
        "CREATE FUNCTION of9() RETURNS int AS $$ SELECT 1 $$ LANGUAGE sql",
    ] {
        e.execute(sql)
            .unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    }
    e
}

#[test]
fn every_object_kind_records_its_new_owner() {
    let mut e = seeded();
    for sql in [
        "ALTER TABLE ot OWNER TO o9",
        "ALTER SEQUENCE os OWNER TO o9",
        "ALTER VIEW ov OWNER TO o9",
        "ALTER MATERIALIZED VIEW omv OWNER TO o9",
        "ALTER TYPE oc OWNER TO o9",
        "ALTER DOMAIN od OWNER TO o9",
        "ALTER FUNCTION of9() OWNER TO o9",
    ] {
        e.execute(sql)
            .unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    }
    for (sql, what) in [
        (
            "SELECT pg_get_userbyid(relowner) FROM pg_class WHERE relname='ot'",
            "table",
        ),
        (
            "SELECT pg_get_userbyid(relowner) FROM pg_class WHERE relname='os'",
            "sequence",
        ),
        (
            "SELECT pg_get_userbyid(relowner) FROM pg_class WHERE relname='ov'",
            "view",
        ),
        (
            "SELECT pg_get_userbyid(relowner) FROM pg_class WHERE relname='omv'",
            "materialized view",
        ),
        (
            "SELECT pg_get_userbyid(typowner) FROM pg_type WHERE typname='oc'",
            "composite type",
        ),
        (
            "SELECT pg_get_userbyid(typowner) FROM pg_type WHERE typname='od'",
            "domain",
        ),
        (
            "SELECT pg_get_userbyid(proowner) FROM pg_proc WHERE proname='of9'",
            "function",
        ),
    ] {
        assert_eq!(col(&mut e, sql), vec!["o9".to_string()], "{what}");
    }
}

#[test]
fn the_two_surfaces_that_read_an_owner_agree() {
    let mut e = seeded();
    e.execute("ALTER MATERIALIZED VIEW omv OWNER TO o9")
        .unwrap();
    e.execute("ALTER VIEW ov OWNER TO o9").unwrap();
    assert_eq!(
        col(
            &mut e,
            "SELECT matviewowner FROM pg_matviews WHERE matviewname='omv'"
        ),
        vec!["o9".to_string()]
    );
    assert_eq!(
        col(&mut e, "SELECT viewowner FROM pg_views WHERE viewname='ov'"),
        vec!["o9".to_string()]
    );
}

#[test]
fn a_name_that_does_not_exist_is_refused_the_way_pg_refuses_it() {
    let mut e = seeded();
    // PG 18.6's own sentences.
    for (sql, want) in [
        (
            "ALTER VIEW nov OWNER TO o9",
            "relation \"nov\" does not exist",
        ),
        (
            "ALTER SEQUENCE nos OWNER TO o9",
            "relation \"nos\" does not exist",
        ),
        ("ALTER TYPE noc OWNER TO o9", "type \"noc\" does not exist"),
        (
            "ALTER FUNCTION nof() OWNER TO o9",
            "function nof() does not exist",
        ),
        (
            "ALTER VIEW ov OWNER TO norole",
            "role \"norole\" does not exist",
        ),
    ] {
        let got = err(&mut e, sql);
        assert!(got.contains(want), "{sql}: {got}");
    }
}
