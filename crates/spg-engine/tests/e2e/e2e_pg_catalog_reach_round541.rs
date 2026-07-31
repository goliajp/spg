//! v7.39 (round 541) — the catalog surface `pg_dump` walks.
//!
//! Round 540 got its first catalog query answered; this one follows the
//! next failures, and each turned out to be a different shape of the
//! same thing — a fact SPG knew but did not publish where PG publishes
//! it.
//!
//! **Writing the schema qualifier changed the answer.** The bare path
//! checked a curated list of catalogs SPG synthesises; the QUALIFIED
//! path checked nothing and rewrote `pg_catalog.<anything>` to
//! `__spg_pg_<anything>`, so:
//!
//!     SELECT count(*) FROM pg_stat_activity              4
//!     SELECT count(*) FROM pg_catalog.pg_stat_activity   ERROR
//!
//! Four views vanished under their own qualified names, and every
//! unknown name got a message about a view SPG could not materialise
//! rather than PG's "relation does not exist". One list now serves both.
//!
//! **pg_class was six columns short of PG18** — relallfrozen,
//! relrewrite, relfrozenxid, relminmxid, reloptions, relpartbound.
//! pg_dump selects three of them by name. reloptions had to be a real
//! `text[]`: pg_dump does `array_remove(c.reloptions, …)` and
//! `… = ANY (c.reloptions)`, which a text column holding
//! `{check_option=local}` fails while printing correctly.
//!
//! **Eighty-four of PG18's 144 pg_catalog relations were absent.** A
//! catalog that exists and is empty says "none here"; one that does not
//! exist stops the tool. Twenty-nine land here — the ones SPG is
//! genuinely empty of.
//!
//! Every expectation below is a PG18 reading.

use spg_engine::{Engine, QueryResult};

fn engine() -> Engine {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (a INT PRIMARY KEY, b TEXT)").unwrap();
    e.execute("CREATE VIEW v AS SELECT a FROM t WHERE a > 0 WITH LOCAL CHECK OPTION")
        .unwrap();
    e.execute("CREATE SEQUENCE s").unwrap();
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

/// A schema qualifier does not change what a catalog answers.
#[test]
fn round541_qualified_and_bare_agree() {
    let mut e = engine();
    for name in [
        // These route through the meta_view_result path under their own
        // names — the ones the qualified rewrite used to mis-target.
        "pg_stat_activity",
        "pg_locks",
        "pg_statio_user_tables",
        // And the ordinary synthesised ones, which already agreed.
        "pg_class",
        "pg_namespace",
        "pg_extension",
    ] {
        let bare = rows(&mut e, &format!("SELECT count(*) FROM {name}"));
        let qual = rows(&mut e, &format!("SELECT count(*) FROM pg_catalog.{name}"));
        assert_eq!(bare, qual, "{name}: qualifying it changed the answer");
    }
}

/// A name that is no catalog gets PG's message, qualified or not.
#[test]
fn round541_unknown_catalog_says_it_does_not_exist() {
    let mut e = engine();
    for sql in [
        "SELECT 1 FROM pg_nonesuch",
        "SELECT 1 FROM pg_catalog.pg_nonesuch",
    ] {
        let err = format!("{}", e.execute(sql).expect_err(sql));
        assert!(
            err.contains("does not exist"),
            "{sql}: message was {err}"
        );
        assert!(
            !err.contains("materialisable"),
            "{sql}: still the internal message — {err}"
        );
    }
}

/// pg_class publishes PG18's thirty-four columns, in PG's order.
#[test]
fn round541_pg_class_has_pg18s_columns() {
    let mut e = engine();
    assert_eq!(
        columns(&mut e, "SELECT * FROM pg_class"),
        vec![
            "oid",
            "relname",
            "relnamespace",
            "reltype",
            "reloftype",
            "relowner",
            "relam",
            "relfilenode",
            "reltablespace",
            "relpages",
            "reltuples",
            "relallvisible",
            "relallfrozen",
            "reltoastrelid",
            "relhasindex",
            "relisshared",
            "relpersistence",
            "relkind",
            "relnatts",
            "relchecks",
            "relhasrules",
            "relhastriggers",
            "relhassubclass",
            "relrowsecurity",
            "relforcerowsecurity",
            "relispopulated",
            "relreplident",
            "relispartition",
            "relrewrite",
            "relfrozenxid",
            "relminmxid",
            "relacl",
            "reloptions",
            "relpartbound",
        ]
    );
}

/// A freeze cutoff exists only where there is heap storage — measured
/// per relkind against PG18.
#[test]
fn round541_frozen_xid_only_where_there_is_storage() {
    let mut e = engine();
    // v7.39 (round 640) — this read used to be `relfrozenxid > 0`, which
    // PG refuses: "operator does not exist: xid > integer". It passed
    // only because SPG typed the column bigint, so the assertion was
    // pinning SPG's own gap as if it were the rule. `<> '0'::xid` is the
    // same question in the operators the type actually has, and both
    // engines answer it.
    let read = |e: &mut Engine, name: &str| {
        rows(
            e,
            &format!(
                "SELECT relkind, relallfrozen, relrewrite, \
                 relfrozenxid <> '0'::xid, relminmxid FROM pg_class WHERE relname = '{name}'"
            ),
        )
    };
    // A table freezes; PG reports a real xid and relminmxid 1.
    assert_eq!(read(&mut e, "t"), vec!["r|0|0|true|1"]);
    // A sequence, an index and a view have no heap to freeze: all zero.
    assert_eq!(read(&mut e, "s"), vec!["S|0|0|false|0"]);
    assert_eq!(read(&mut e, "t_pkey"), vec!["i|0|0|false|0"]);
    assert_eq!(read(&mut e, "v"), vec!["v|0|0|false|0"]);
}

/// reloptions carries a view's check option, as a real array.
#[test]
fn round541_reloptions_is_an_array_pg_dump_can_use() {
    let mut e = engine();
    e.execute("CREATE VIEW vc AS SELECT a FROM t WHERE a > 0 WITH CASCADED CHECK OPTION")
        .unwrap();
    assert_eq!(
        rows(
            &mut e,
            "SELECT relname, reloptions FROM pg_class \
             WHERE relname IN ('v', 'vc') ORDER BY relname"
        ),
        vec!["v|{check_option=local}", "vc|{check_option=cascaded}"]
    );
    // The two array operations pg_dump actually applies to it.
    assert_eq!(
        rows(
            &mut e,
            "SELECT 'check_option=local' = ANY (reloptions), \
             array_remove(reloptions, 'check_option=local') \
             FROM pg_class WHERE relname = 'v'"
        ),
        vec!["true|{}"]
    );
    // A relation with no options reports NULL, not an empty array.
    assert_eq!(
        rows(&mut e, "SELECT reloptions FROM pg_class WHERE relname = 't'"),
        vec!["NULL"]
    );
}

/// The catalogs SPG is genuinely empty of exist and answer no rows.
#[test]
fn round541_empty_catalogs_exist() {
    let mut e = engine();
    for name in [
        "pg_init_privs",
        "pg_foreign_table",
        "pg_foreign_data_wrapper",
        "pg_foreign_server",
        "pg_user_mapping",
        "pg_user_mappings",
        "pg_event_trigger",
        "pg_seclabel",
        "pg_shseclabel",
        "pg_seclabels",
        "pg_publication_rel",
        "pg_publication_namespace",
        "pg_publication_tables",
        "pg_subscription_rel",
        "pg_replication_origin",
        "pg_replication_origin_status",
        "pg_transform",
        "pg_parameter_acl",
        "pg_prepared_xacts",
        "pg_shdepend",
        "pg_shdescription",
        "pg_statistic_ext_data",
        "pg_stats_ext",
        "pg_stats_ext_exprs",
        "pg_file_settings",
        "pg_hba_file_rules",
        "pg_ident_file_mappings",
        "pg_shmem_allocations",
        "pg_shmem_allocations_numa",
    ] {
        assert_eq!(
            rows(&mut e, &format!("SELECT count(*) FROM {name}")),
            vec!["0"],
            "{name} should exist and be empty"
        );
    }
}

/// And they carry PG's columns, not a placeholder.
#[test]
fn round541_empty_catalogs_have_pgs_columns() {
    let mut e = engine();
    assert_eq!(
        columns(&mut e, "SELECT * FROM pg_foreign_server"),
        vec!["oid", "srvname", "srvowner", "srvfdw", "srvtype", "srvversion", "srvacl", "srvoptions"]
    );
    assert_eq!(
        columns(&mut e, "SELECT * FROM pg_catalog.pg_init_privs"),
        vec!["objoid", "classoid", "objsubid", "privtype", "initprivs"]
    );
    assert_eq!(
        columns(&mut e, "SELECT * FROM pg_foreign_table"),
        vec!["ftrelid", "ftserver", "ftoptions"]
    );
}

/// The subselect `pg_dump` writes over pg_foreign_table for every
/// relation of kind 'f'.
#[test]
fn round541_pg_dumps_foreignserver_subselect() {
    let mut e = engine();
    assert_eq!(
        rows(
            &mut e,
            "SELECT CASE WHEN c.relkind = 'f' \
             THEN (SELECT ftserver FROM pg_catalog.pg_foreign_table WHERE ftrelid = c.oid) \
             ELSE 0 END FROM pg_class c WHERE c.relname = 't'"
        ),
        vec!["0"]
    );
}
