//! v7.39 (round 542) — the catalog is there, the columns are somebody
//! else's.
//!
//! Round 541 registered the catalogs SPG was missing. Running pg_dump
//! against that then failed on a COLUMN, not a relation, so this round
//! diffed every catalog SPG publishes against PG18's column set:
//! twenty-six catalogs, 179 columns short between them.
//!
//! Three of those are worse than short — they publish somebody else's
//! columns under PG's name:
//!
//!     pg_user       had pg_roles' columns, so `SELECT usename
//!                   FROM pg_user` — the plainest way there is to ask
//!                   who can log in — failed
//!     pg_matviews   had pg_views' columns (`viewname`, not
//!                   `matviewname`) AND was pinned empty, with a note
//!                   saying SPG has no materialized views; it has had
//!                   them since round 338
//!     pg_trigger    had information_schema.triggers' column names
//!                   (relname / timing / events / function), so every
//!                   pg_catalog trigger query failed
//!
//! A missing catalog returns "relation does not exist" and the reader
//! knows the database lacks it. A catalog with the wrong columns
//! returns "column does not exist", and the reader concludes their own
//! SQL is wrong.
//!
//! Every expectation below is a PG18 reading.

use spg_engine::{Engine, QueryResult};

fn engine() -> Engine {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (a INT PRIMARY KEY, b TEXT)").unwrap();
    e.execute("CREATE VIEW v AS SELECT a FROM t WHERE a > 0").unwrap();
    e.execute("CREATE MATERIALIZED VIEW mv AS SELECT a FROM t").unwrap();
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

/// pg_user is its own view, with PG's `use*` names.
#[test]
fn round542_pg_user_has_pgs_columns() {
    let mut e = engine();
    assert_eq!(
        columns(&mut e, "SELECT * FROM pg_user"),
        vec![
            "usename",
            "usesysid",
            "usecreatedb",
            "usesuper",
            "userepl",
            "usebypassrls",
            "passwd",
            "valuntil",
            "useconfig",
        ]
    );
    // The bootstrap superuser, shaped as PG shapes one.
    assert_eq!(
        rows(&mut e, "SELECT * FROM pg_user WHERE usename = 'postgres'"),
        vec!["postgres|10|true|true|true|true|********|NULL|NULL"]
    );
    // The password never leaves the catalog — PG's view exists for that.
    //
    // v7.39 (round 696) — TWO rows now: the bootstrap superuser and the
    // session's own identity. `current_user` always reported that identity;
    // until this round nothing else did, so `SET ROLE <me>` refused the role
    // the session was already running as. Masked either way, which is what
    // this pin is actually about.
    assert_eq!(
        rows(&mut e, "SELECT passwd FROM pg_user"),
        vec!["********", "********"]
    );
}

/// Only roles that can log in appear in pg_user; all of them in pg_roles.
#[test]
fn round542_pg_user_lists_only_login_roles() {
    let mut e = engine();
    e.execute("CREATE ROLE app LOGIN PASSWORD 'x'").unwrap();
    e.execute("CREATE ROLE devs NOLOGIN").unwrap();
    let users = rows(&mut e, "SELECT usename FROM pg_user ORDER BY usename");
    assert!(users.contains(&"app".to_string()), "{users:?}");
    assert!(!users.contains(&"devs".to_string()), "{users:?}");
    let roles = rows(&mut e, "SELECT rolname FROM pg_roles ORDER BY rolname");
    assert!(roles.contains(&"devs".to_string()), "{roles:?}");
}

/// pg_roles publishes PG18's thirteen columns, with oid LAST.
#[test]
fn round542_pg_roles_has_pgs_columns() {
    let mut e = engine();
    assert_eq!(
        columns(&mut e, "SELECT * FROM pg_roles"),
        vec![
            "rolname",
            "rolsuper",
            "rolinherit",
            "rolcreaterole",
            "rolcreatedb",
            "rolcanlogin",
            "rolreplication",
            "rolconnlimit",
            "rolpassword",
            "rolvaliduntil",
            "rolbypassrls",
            "rolconfig",
            "oid",
        ]
    );
    // A superuser bypasses every check the four unrecorded attributes
    // gate, which is why PG reports them true for one.
    assert_eq!(
        rows(&mut e, "SELECT * FROM pg_roles WHERE rolname = 'postgres'"),
        vec!["postgres|true|true|true|true|true|true|-1|********|NULL|true|NULL|10"]
    );
    // A plain role gets none of them.
    e.execute("CREATE ROLE plain NOLOGIN").unwrap();
    assert_eq!(
        rows(
            &mut e,
            "SELECT rolsuper, rolcreatedb, rolcreaterole, rolreplication, \
             rolbypassrls, rolcanlogin, rolconnlimit \
             FROM pg_roles WHERE rolname = 'plain'"
        ),
        vec!["false|false|false|false|false|false|-1"]
    );
}

/// pg_matviews lists materialized views, under PG's column names.
#[test]
fn round542_pg_matviews_has_rows_and_pgs_columns() {
    let mut e = engine();
    assert_eq!(
        columns(&mut e, "SELECT * FROM pg_matviews"),
        vec![
            "schemaname",
            "matviewname",
            "matviewowner",
            "tablespace",
            "hasindexes",
            "ispopulated",
            "definition",
        ]
    );
    assert_eq!(
        rows(
            &mut e,
            "SELECT schemaname, matviewname, matviewowner, tablespace, \
             hasindexes, ispopulated FROM pg_matviews"
        ),
        vec!["public|mv|postgres|NULL|false|true"]
    );
    // An ordinary view is not one.
    assert_eq!(
        rows(&mut e, "SELECT count(*) FROM pg_matviews WHERE matviewname = 'v'"),
        vec!["0"]
    );
    // And an index on it shows.
    e.execute("CREATE INDEX mvix ON mv (a)").unwrap();
    assert_eq!(
        rows(&mut e, "SELECT hasindexes FROM pg_matviews"),
        vec!["true"]
    );
}

/// pg_views gained viewowner, pg_indexes gained tablespace.
#[test]
fn round542_view_and_index_listings_are_complete() {
    let mut e = engine();
    assert_eq!(
        columns(&mut e, "SELECT * FROM pg_views"),
        vec!["schemaname", "viewname", "viewowner", "definition"]
    );
    assert_eq!(
        rows(
            &mut e,
            "SELECT schemaname, viewname, viewowner FROM pg_views WHERE viewname = 'v'"
        ),
        vec!["public|v|postgres"]
    );
    assert_eq!(
        columns(&mut e, "SELECT * FROM pg_indexes"),
        vec!["schemaname", "tablename", "indexname", "tablespace", "indexdef"]
    );
    // NULL means the default tablespace, which is the only one SPG has.
    assert_eq!(
        rows(
            &mut e,
            "SELECT tablespace FROM pg_indexes WHERE indexname = 't_pkey'"
        ),
        vec!["NULL"]
    );
}

/// pg_trigger publishes PG18's nineteen columns, and tgtype is the
/// bitmask a tool reads instead of parsing text.
#[test]
fn round542_pg_trigger_has_pgs_columns_and_bitmask() {
    let mut e = engine();
    e.execute("CREATE FUNCTION f() RETURNS trigger AS 'BEGIN RETURN NEW; END' LANGUAGE plpgsql")
        .unwrap();
    e.execute("CREATE TRIGGER tg BEFORE INSERT ON t FOR EACH ROW EXECUTE FUNCTION f()")
        .unwrap();
    assert_eq!(
        columns(&mut e, "SELECT * FROM pg_trigger"),
        vec![
            "oid",
            "tgrelid",
            "tgparentid",
            "tgname",
            "tgfoid",
            "tgtype",
            "tgenabled",
            "tgisinternal",
            "tgconstrrelid",
            "tgconstrindid",
            "tgconstraint",
            "tgdeferrable",
            "tginitdeferred",
            "tgnargs",
            "tgattr",
            "tgargs",
            "tgqual",
            "tgoldtable",
            "tgnewtable",
        ]
    );
    // 1 ROW | 2 BEFORE | 4 INSERT = 7, measured on PG18 for this shape.
    assert_eq!(
        rows(
            &mut e,
            "SELECT tgname, tgtype, tgenabled, tgisinternal, tgnargs, tgargs \
             FROM pg_trigger WHERE tgname = 'tg'"
        ),
        vec!["tg|7|O|false|0|\\x"]
    );
    // The canonical join, which the old column names made impossible.
    assert_eq!(
        rows(
            &mut e,
            "SELECT c.relname, t.tgname FROM pg_trigger t \
             JOIN pg_class c ON c.oid = t.tgrelid"
        ),
        vec!["t|tg"]
    );
}

/// Each event sets its own bit, and AFTER clears the BEFORE one.
#[test]
fn round542_tgtype_bits_per_event() {
    let mut e = engine();
    e.execute("CREATE FUNCTION f() RETURNS trigger AS 'BEGIN RETURN NEW; END' LANGUAGE plpgsql")
        .unwrap();
    e.execute("CREATE TRIGGER a1 AFTER UPDATE ON t FOR EACH ROW EXECUTE FUNCTION f()")
        .unwrap();
    e.execute("CREATE TRIGGER a2 AFTER INSERT OR DELETE ON t FOR EACH ROW EXECUTE FUNCTION f()")
        .unwrap();
    assert_eq!(
        rows(
            &mut e,
            "SELECT tgname, tgtype FROM pg_trigger WHERE tgname IN ('a1', 'a2') ORDER BY tgname"
        ),
        // 1 ROW | 16 UPDATE = 17; 1 ROW | 4 INSERT | 8 DELETE = 13.
        vec!["a1|17", "a2|13"]
    );
}
