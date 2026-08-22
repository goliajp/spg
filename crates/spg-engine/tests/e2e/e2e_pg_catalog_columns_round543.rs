//! v7.39 (round 543) — the structural half of round 542's 179 columns.
//!
//! Round 542 measured the gap and fixed the three catalogs publishing
//! somebody else's columns. This one fills the columns a tool reads to
//! learn the SHAPE of a schema — the nine catalogs pg_dump and every
//! introspecting client walk — leaving the monitoring surface
//! (pg_stat_*, pg_locks, pg_statistic) for its own round.
//!
//! Two of the forty-two are not filler:
//!
//!   * `pg_constraint.conbin` is what a tool tests to know a constraint
//!     is a CHECK. Measured on PG18: non-NULL for contype 'c' and NULL
//!     for every other kind.
//!   * `pg_index.indexprs` / `indpred` are how an expression index and
//!     a partial index announce themselves. Both NULL for a plain
//!     index, both non-NULL for `ON t ((a+1)) WHERE b <> ''`.
//!
//! and `pg_proc.proargnames` is simply real — CREATE FUNCTION records
//! its parameter names and every named-argument caller reads them here.
//!
//! Round 542 shipped a schema widened without its row builder twice
//! before a probe caught it, so `materialise_meta_view` now checks that
//! every row is as wide as the schema. It found a pre-existing one on
//! its first run: `information_schema.schemata` carried TEN values
//! against a seven-column schema.
//!
//! Every expectation below is a PG18 reading.

use spg_engine::{Engine, QueryResult};

fn engine() -> Engine {
    let mut e = Engine::new();
    e.execute(
        "CREATE TABLE t (a INT PRIMARY KEY, b TEXT NOT NULL DEFAULT 'x', c INT CHECK (c > 0))",
    )
    .unwrap();
    e.execute("CREATE INDEX ixe ON t ((a + 1)) WHERE b <> ''")
        .unwrap();
    e.execute("CREATE FUNCTION fx(p integer, q text) RETURNS integer AS 'SELECT 1' LANGUAGE sql")
        .unwrap();
    e
}

fn rows(e: &mut Engine, sql: &str) -> Vec<String> {
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

fn columns(e: &mut Engine, sql: &str) -> Vec<String> {
    match e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err}")) {
        QueryResult::Rows { columns, .. } => columns.iter().map(|c| c.name.clone()).collect(),
        other => panic!("{sql}: {other:?}"),
    }
}

/// conbin discriminates a CHECK from everything else, as PG's does.
#[test]
fn round543_conbin_marks_a_check_constraint() {
    let mut e = engine();
    assert_eq!(
        rows(
            &mut e,
            "SELECT conname, contype, conenforced, conperiod, conbin \
             FROM pg_constraint WHERE conrelid = 't'::regclass ORDER BY conname"
        ),
        vec![
            "t_a_not_null|n|true|false|NULL",
            "t_b_not_null|n|true|false|NULL",
            // The one kind PG gives a non-NULL conbin.
            "t_c_check|c|true|false|(c > 0)",
            "t_pkey|p|true|false|NULL",
        ]
    );
}

/// indexprs / indpred announce an expression index and a partial one.
#[test]
fn round543_index_expression_and_predicate() {
    let mut e = engine();
    assert_eq!(
        rows(
            &mut e,
            "SELECT c.relname, i.indexprs, i.indpred FROM pg_index i \
             JOIN pg_class c ON c.oid = i.indexrelid ORDER BY c.relname"
        ),
        vec![
            "ixe|(a + 1)|(b <> '')",
            // A plain index has neither, which is how a tool tells.
            "t_pkey|NULL|NULL",
        ]
    );
}

/// proargnames carries the declared parameter names.
#[test]
fn round543_proargnames_is_real() {
    let mut e = engine();
    assert_eq!(
        rows(
            &mut e,
            "SELECT proname, proargnames FROM pg_proc WHERE proname = 'fx'"
        ),
        vec!["fx|{p,q}"]
    );
    // A function that takes none reports NULL, as PG's does.
    e.execute("CREATE FUNCTION fy() RETURNS integer AS 'SELECT 1' LANGUAGE sql")
        .unwrap();
    assert_eq!(
        rows(
            &mut e,
            "SELECT proargnames FROM pg_proc WHERE proname = 'fy'"
        ),
        vec!["NULL"]
    );
}

/// pg_type reaches PG18's thirty-two columns; typacl is what pg_dump
/// selects by name.
#[test]
fn round543_pg_type_is_complete() {
    let mut e = engine();
    let cols = columns(&mut e, "SELECT * FROM pg_type");
    assert_eq!(cols.len(), 32, "{cols:?}");
    assert_eq!(
        &cols[15..22],
        &[
            "typinput",
            "typoutput",
            "typreceive",
            "typsend",
            "typmodin",
            "typmodout",
            "typanalyze"
        ]
    );
    assert_eq!(&cols[29..], &["typdefaultbin", "typdefault", "typacl"]);
    // SPG's type I/O is built into the engine and is not a catalogued
    // function, so there is nothing for these to name — 0 is the value
    // PG itself uses for a type with no such function.
    assert_eq!(
        rows(
            &mut e,
            "SELECT typinput, typoutput, typdefault, typacl FROM pg_type WHERE typname = 'int4'"
        ),
        vec!["0|0|NULL|NULL"]
    );
}

/// The other five reach PG18's column count too.
#[test]
fn round543_the_rest_reach_pgs_width() {
    let mut e = engine();
    for (sql, want) in [
        ("SELECT * FROM pg_proc", 30),
        ("SELECT * FROM pg_constraint", 28),
        ("SELECT * FROM pg_attribute", 25),
        ("SELECT * FROM pg_index", 21),
        ("SELECT * FROM pg_collation", 12),
        ("SELECT * FROM pg_publication", 10),
        ("SELECT * FROM pg_auth_members", 7),
        ("SELECT * FROM pg_statistic_ext", 9),
    ] {
        let cols = columns(&mut e, sql);
        assert_eq!(cols.len(), want, "{sql}: {cols:?}");
    }
}

/// The tail values, measured.
#[test]
fn round543_tail_values_match_pg() {
    let mut e = engine();
    assert_eq!(
        rows(
            &mut e,
            "SELECT attname, attcompression, atthasmissing, attoptions \
             FROM pg_attribute WHERE attrelid = 't'::regclass AND attnum > 0 ORDER BY attnum"
        ),
        vec!["a||false|NULL", "b||false|NULL", "c||false|NULL"]
    );
    // PG18 reads all three NULL for C / POSIX / default; a name appears
    // only for an ICU collation.
    //
    // v7.38.18 (G1) — this listed the three rows the catalogue had.
    // It has 880 now, so the same claim is checked as a PROPERTY over
    // all of them, which is what the sentence above was always saying.
    assert_eq!(
        rows(
            &mut e,
            "SELECT collname, colllocale, collicurules, collversion \
             FROM pg_collation WHERE collname IN ('C','POSIX','default') ORDER BY collname"
        ),
        vec![
            "C|NULL|NULL|NULL",
            "POSIX|NULL|NULL|NULL",
            "default|NULL|NULL|NULL"
        ]
    );
    // Every libc-provider row reads NULL there, and every ICU one
    // carries a locale. Measured on PG 18.4 across the whole catalogue.
    assert_eq!(
        rows(
            &mut e,
            "SELECT count(*) FROM pg_collation \
             WHERE collprovider = 'i' AND colllocale IS NULL"
        ),
        vec!["0"]
    );
    assert_eq!(
        rows(
            &mut e,
            "SELECT count(*) FROM pg_collation \
             WHERE collprovider = 'c' AND colllocale IS NOT NULL"
        ),
        vec!["0"]
    );
    // `collicurules` is NULL for every one of them, which is PG's
    // reading for a collation with no custom tailoring.
    assert_eq!(
        rows(
            &mut e,
            "SELECT count(*) FROM pg_collation WHERE collicurules IS NOT NULL"
        ),
        vec!["0"]
    );
}

/// information_schema.schemata is as wide as it says it is.
#[test]
fn round543_schemata_row_width() {
    let mut e = engine();
    let cols = columns(&mut e, "SELECT * FROM information_schema.schemata");
    assert_eq!(cols.len(), 7, "{cols:?}");
    assert_eq!(
        rows(
            &mut e,
            "SELECT catalog_name, schema_name, sql_path \
             FROM information_schema.schemata ORDER BY schema_name"
        ),
        vec![
            "spg|information_schema|NULL",
            "spg|pg_catalog|NULL",
            "spg|public|NULL",
        ]
    );
}
