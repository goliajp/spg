//! v7.40.11 — `pg_get_indexdef(oid)` described a different table's
//! index.
//!
//! Reported against 7.40.9 (§3.17). No join, no parameters, one
//! function call, and the answer belongs to another table — consistently
//! one along in creation order:
//!
//! ```text
//!   ask for the indexes of release_artifacts
//!     release_artifacts_pkey  ->  CREATE UNIQUE INDEX releases_pkey
//!                                 ON public.releases USING btree (id)
//! ```
//!
//! The `relname` beside it is right, because it comes from `pg_class`.
//! Only the function is wrong, so anything that reads a schema back — a
//! migration tool, a schema diff, a dump, an operator checking what is
//! actually indexed — is told about the wrong table with nothing to
//! notice.
//!
//! One name, two spaces. `catalog_indexes` assigns the OIDs that
//! `pg_index`/`pg_class` report: it walks `visible_table_names()` and
//! SKIPS the probe indexes SPG derives for a constraint's non-leading
//! columns, which PostgreSQL has no equivalent of. The function's
//! reverse walk replayed the count over `table_names()` and skipped
//! nothing, so every derived index shifted the mapping by one. It also
//! meant `pg_get_indexdef` could NAME one of those derived indexes,
//! which `pg_index` does not list at all.
//!
//! The reverse now reads the same list the forward direction produces.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn texts(eng: &mut Engine, sql: &str) -> Vec<String> {
    match eng.execute(sql).unwrap_or_else(|e| panic!("{sql}: {e:?}")) {
        QueryResult::Rows { rows, .. } => rows
            .into_iter()
            .map(|r| match &r.values[0] {
                Value::Text(t) => t.to_string(),
                Value::Null => String::from("<null>"),
                other => panic!("{sql}: {other:?}"),
            })
            .collect(),
        other => panic!("{sql}: {other:?}"),
    }
}

fn fixture() -> Engine {
    let mut eng = Engine::new();
    for sql in [
        // Three tables in creation order, each with a differently named
        // key column, so an answer that belongs to a neighbour reads as
        // an obviously different sentence.
        "CREATE TABLE ixa (aid INT PRIMARY KEY, v TEXT)",
        // A table-level UNIQUE whose derived probe indexes are exactly
        // what shifted the mapping.
        "CREATE TABLE ixb (bid INT PRIMARY KEY, p INT NOT NULL, k TEXT NOT NULL, \
         UNIQUE (p, k))",
        "CREATE TABLE ixc (cid INT PRIMARY KEY)",
    ] {
        eng.execute(sql)
            .unwrap_or_else(|e| panic!("setup {sql:?}: {e:?}"));
    }
    eng
}

/// The reported shape: the OID that `pg_index` reports for an index,
/// handed straight back to `pg_get_indexdef`.
#[test]
fn the_definition_belongs_to_the_index_the_oid_names() {
    let mut eng = fixture();
    let got = texts(
        &mut eng,
        "SELECT c2.relname || ' => ' || pg_get_indexdef(i.indexrelid) \
         FROM pg_class c \
         JOIN pg_index i ON c.oid = i.indrelid \
         JOIN pg_class c2 ON i.indexrelid = c2.oid \
         ORDER BY 1",
    );
    assert!(!got.is_empty(), "no indexes described at all");
    for line in &got {
        let (name, def) = line.split_once(" => ").unwrap_or_else(|| panic!("{line}"));
        assert!(
            def.contains(&format!(" INDEX {name} ")),
            "the definition must be this index's: {line}"
        );
    }
}

/// And the definition names the index's own TABLE, which is the half
/// the reporter saw: `release_artifacts_pkey` described `releases`.
#[test]
fn the_definition_names_its_own_table() {
    let mut eng = fixture();
    for (table, index, column) in [
        ("ixa", "ixa_pkey", "aid"),
        ("ixb", "ixb_pkey", "bid"),
        ("ixc", "ixc_pkey", "cid"),
    ] {
        let got = texts(
            &mut eng,
            &format!(
                "SELECT pg_get_indexdef(i.indexrelid) FROM pg_index i \
                 JOIN pg_class c2 ON i.indexrelid = c2.oid \
                 WHERE c2.relname = '{index}'"
            ),
        );
        assert_eq!(got.len(), 1, "{index}");
        assert!(
            got[0].contains(&format!("ON public.{table} "))
                && got[0].contains(&format!("({column})")),
            "{index}: {}",
            got[0]
        );
    }
}

/// An OID that names nothing answers NULL rather than a neighbour's
/// definition — the failure mode this had was silence, not an error.
#[test]
fn an_oid_that_names_nothing_is_null() {
    let mut eng = fixture();
    assert_eq!(
        texts(&mut eng, "SELECT pg_get_indexdef(999999)"),
        vec!["<null>".to_string()]
    );
}

/// The derived probe indexes are not in `pg_index`, so the function
/// must not name one either. `ixb`'s `UNIQUE (p, k)` builds one on `k`
/// that PostgreSQL has no equivalent of.
#[test]
fn a_derived_probe_index_is_not_addressable() {
    let mut eng = fixture();
    let listed = texts(
        &mut eng,
        "SELECT c2.relname FROM pg_index i JOIN pg_class c2 ON i.indexrelid = c2.oid ORDER BY 1",
    );
    assert!(
        !listed.iter().any(|n| n.contains("_key_0_")),
        "pg_index lists a derived probe index: {listed:?}"
    );
    let described = texts(
        &mut eng,
        "SELECT coalesce(pg_get_indexdef(i.indexrelid), '<null>') \
         FROM pg_index i ORDER BY 1",
    );
    assert!(
        !described.iter().any(|d| d.contains("_key_0_")),
        "a definition names a derived probe index: {described:?}"
    );
}

/// The name form, which already worked, still does — the two spellings
/// are one answer.
#[test]
fn the_name_form_and_the_oid_form_agree() {
    let mut eng = fixture();
    let by_oid = texts(
        &mut eng,
        "SELECT pg_get_indexdef(i.indexrelid) FROM pg_index i \
         JOIN pg_class c2 ON i.indexrelid = c2.oid WHERE c2.relname = 'ixb_pkey'",
    );
    let by_name = texts(&mut eng, "SELECT pg_get_indexdef('ixb_pkey'::regclass)");
    assert_eq!(by_oid, by_name);
}

/// Found by fixing the OID form: the NAME form answered NULL for every
/// table-level UNIQUE. `pg_index` lists the constraint's index as
/// `ixb_p_k_key`; storage calls the B-tree behind it `ixb_p_key_0_0`,
/// and the function searched storage for the catalog's name.
///
/// So `SELECT pg_get_indexdef('ixb_p_k_key'::regclass)` — the spelling
/// psql's `\d` uses — was silent about the one index a reader most
/// wants described.
#[test]
fn a_constraints_own_index_describes_itself() {
    let mut eng = fixture();
    let got = texts(&mut eng, "SELECT pg_get_indexdef('ixb_p_k_key'::regclass)");
    assert_eq!(got.len(), 1);
    assert!(
        got[0].contains("CREATE UNIQUE INDEX ixb_p_k_key ON public.ixb")
            && got[0].contains("(p, k)"),
        "{}",
        got[0]
    );
    // And it agrees with what the catalog view says about the same one.
    let view = texts(
        &mut eng,
        "SELECT indexdef FROM pg_indexes WHERE indexname = 'ixb_p_k_key'",
    );
    assert_eq!(view, got, "the view and the function are one answer");
}

/// The column form, over an index the catalog names but storage does
/// not: the N-th key column, 1-based, NULL past the end.
#[test]
fn the_column_form_reads_the_catalogs_key_list() {
    let mut eng = fixture();
    assert_eq!(
        texts(
            &mut eng,
            "SELECT pg_get_indexdef('ixb_p_k_key'::regclass, 1, true)"
        ),
        vec!["p".to_string()]
    );
    assert_eq!(
        texts(
            &mut eng,
            "SELECT pg_get_indexdef('ixb_p_k_key'::regclass, 2, true)"
        ),
        vec!["k".to_string()]
    );
    assert_eq!(
        texts(
            &mut eng,
            "SELECT pg_get_indexdef('ixb_p_k_key'::regclass, 3, true)"
        ),
        vec!["<null>".to_string()]
    );
}
