//! v7.39 (round 546) — the catalogs SPG has real content for.
//!
//! Round 541 split the 84 missing pg_catalog relations into the ones
//! SPG is genuinely empty of (done then, table-driven) and the ones
//! that would NOT be empty — stubbing those would have been a lie. This
//! round does seven of the second kind, each built from a fact SPG
//! already holds so none of them can drift from it:
//!
//!     pg_language            the languages a CREATE FUNCTION can name
//!     pg_sequences           the listing view over pg_sequence
//!     pg_range               SPG ships PG18's same six range types
//!     pg_partitioned_table   one row per partition PARENT
//!     pg_authid / pg_group / pg_shadow   from synth_pg_roles' rows
//!
//! and three more that ARE genuinely empty: SPG has no ALTER DEFAULT
//! PRIVILEGES, one encoding (so no conversions between any), and it
//! records no per-role or per-database GUC settings.
//!
//! Two deliberate divergences, recorded rather than silently taken:
//!
//!   * `c` is NOT in pg_language. SPG cannot load a shared object, and
//!     a row here would claim it could.
//!   * pg_authid and pg_shadow MASK the password. PG guards the real
//!     hash with catalog privileges SPG does not have, so publishing a
//!     SCRAM verifier there would put it within reach of any session.
//!
//! Every expectation below is a PG18 reading.

use spg_engine::{Engine, QueryResult};

fn engine() -> Engine {
    let mut e = Engine::new();
    e.execute("CREATE SEQUENCE s1 START 5 INCREMENT 2 MINVALUE 1 MAXVALUE 99 CACHE 3 CYCLE")
        .unwrap();
    e.execute("CREATE TABLE p1 (a INT, b TEXT) PARTITION BY RANGE (a)")
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

/// The languages SPG can actually run — and not the one it cannot.
#[test]
fn round546_pg_language_lists_what_runs() {
    let mut e = engine();
    assert_eq!(
        rows(
            &mut e,
            "SELECT oid, lanname, lanispl, lanpltrusted FROM pg_language ORDER BY oid"
        ),
        // PG's own oids, measured: internal 12, sql 14, plpgsql 13647.
        vec![
            "12|internal|false|false",
            "14|sql|false|true",
            "13647|plpgsql|true|true",
        ]
    );
    // `c` is PG's fourth and is deliberately absent.
    assert_eq!(
        rows(&mut e, "SELECT count(*) FROM pg_language WHERE lanname = 'c'"),
        vec!["0"]
    );
    // The join every function browser makes.
    assert_eq!(
        rows(
            &mut e,
            "SELECT l.lanname FROM pg_proc p JOIN pg_language l ON l.oid = p.prolang \
             WHERE p.proname = 'abs' LIMIT 1"
        ),
        vec!["internal"]
    );
}

/// pg_sequences reports the definition, not a placeholder.
#[test]
fn round546_pg_sequences_reads_the_definition() {
    let mut e = engine();
    assert_eq!(
        columns(&mut e, "SELECT * FROM pg_sequences"),
        vec![
            "schemaname",
            "sequencename",
            "sequenceowner",
            "data_type",
            "start_value",
            "min_value",
            "max_value",
            "increment_by",
            "cycle",
            "cache_size",
            "last_value",
        ]
    );
    // last_value is NULL until the sequence has been called, as PG's is.
    // sequenceowner is the role that ran CREATE SEQUENCE, which is the
    // session's own — `admin` embedded, `postgres` over the wire — so
    // the pin reads it from the same place rather than naming one.
    assert_eq!(
        rows(
            &mut e,
            "SELECT schemaname, sequencename, sequenceowner = current_user, data_type, \
             start_value, min_value, max_value, increment_by, cycle, cache_size, last_value \
             FROM pg_sequences"
        ),
        vec!["public|s1|true|bigint|5|1|99|2|true|3|NULL"]
    );
    e.execute("SELECT nextval('s1')").unwrap();
    assert_eq!(
        rows(&mut e, "SELECT last_value FROM pg_sequences"),
        vec!["5"]
    );
    // And it agrees with the raw catalog it is derived from.
    assert_eq!(
        rows(
            &mut e,
            "SELECT s.seqstart, s.seqincrement FROM pg_sequence s \
             JOIN pg_class c ON c.oid = s.seqrelid WHERE c.relname = 's1'"
        ),
        vec!["5|2"]
    );
}

/// The six range types, keyed to the oids pg_type already publishes.
#[test]
fn round546_pg_range_is_the_six() {
    let mut e = engine();
    assert_eq!(
        rows(
            &mut e,
            "SELECT t.typname, s.typname FROM pg_range r \
             JOIN pg_type t ON t.oid = r.rngtypid \
             JOIN pg_type s ON s.oid = r.rngsubtype ORDER BY t.typname"
        ),
        vec![
            "daterange|date",
            "int4range|int4",
            "int8range|int8",
            "numrange|numeric",
            "tsrange|timestamp",
            "tstzrange|timestamptz",
        ]
    );
}

/// A partition parent, with PG's strategy char and key vector.
#[test]
fn round546_pg_partitioned_table() {
    let mut e = engine();
    assert_eq!(
        rows(
            &mut e,
            "SELECT c.relname, p.partstrat, p.partnatts, p.partattrs, p.partexprs \
             FROM pg_partitioned_table p JOIN pg_class c ON c.oid = p.partrelid"
        ),
        // partattrs is 1-based, as PG's attnums are.
        vec!["p1|r|1|1|NULL"]
    );
    // A LIST parent reports 'l'.
    e.execute("CREATE TABLE p2 (a TEXT, b INT) PARTITION BY LIST (a)")
        .unwrap();
    assert_eq!(
        rows(
            &mut e,
            "SELECT c.relname, p.partstrat FROM pg_partitioned_table p \
             JOIN pg_class c ON c.oid = p.partrelid WHERE c.relname = 'p2'"
        ),
        vec!["p2|l"]
    );
    // An ordinary table is not one.
    e.execute("CREATE TABLE plain (a INT)").unwrap();
    assert_eq!(
        rows(
            &mut e,
            "SELECT count(*) FROM pg_partitioned_table p JOIN pg_class c \
             ON c.oid = p.partrelid WHERE c.relname = 'plain'"
        ),
        vec!["0"]
    );
}

/// The three role views agree with pg_roles, and mask the password.
#[test]
fn round546_role_views_agree_and_mask() {
    let mut e = engine();
    assert_eq!(
        columns(&mut e, "SELECT * FROM pg_authid"),
        vec![
            "oid",
            "rolname",
            "rolsuper",
            "rolinherit",
            "rolcreaterole",
            "rolcreatedb",
            "rolcanlogin",
            "rolreplication",
            "rolbypassrls",
            "rolconnlimit",
            "rolpassword",
            "rolvaliduntil",
        ]
    );
    assert_eq!(
        rows(
            &mut e,
            "SELECT rolname, rolsuper, rolcanlogin, rolconnlimit, rolpassword FROM pg_authid"
        ),
        vec!["postgres|true|true|-1|********"]
    );
    assert_eq!(
        columns(&mut e, "SELECT * FROM pg_group"),
        vec!["groname", "grosysid", "grolist"]
    );
    assert_eq!(
        rows(&mut e, "SELECT usename, passwd FROM pg_shadow"),
        vec!["postgres|********"]
    );
    // pg_authid's oid is pg_roles' oid — the join a tool makes.
    assert_eq!(
        rows(
            &mut e,
            "SELECT a.rolname FROM pg_authid a JOIN pg_roles r ON r.oid = a.oid"
        ),
        vec!["postgres"]
    );
}

/// Three that exist and are empty, because SPG has none of the thing.
#[test]
fn round546_three_more_empty_catalogs() {
    let mut e = engine();
    for name in ["pg_default_acl", "pg_conversion", "pg_db_role_setting"] {
        assert_eq!(
            rows(&mut e, &format!("SELECT count(*) FROM {name}")),
            vec!["0"],
            "{name}"
        );
    }
    assert_eq!(
        columns(&mut e, "SELECT * FROM pg_db_role_setting"),
        vec!["setdatabase", "setrole", "setconfig"]
    );
}
