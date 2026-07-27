//! v7.39 (round 536) — a materialized view is not in `information_schema`.
//!
//! Round 535's keyword-token trap dropped a name in three separate
//! option loops, so this round asked the parser mechanically: which
//! comparisons test an identifier against a word that LEXES as a
//! keyword, and therefore can never fire? Forty-eight of them. Probing
//! the shapes they guard found the rest benign — the grammar has a
//! keyword branch beside them — and one real family, recorded below in
//! the audit rather than fixed here.
//!
//! What this pins is a divergence found alongside:
//!
//!     information_schema.columns WHERE table_name = <matview>
//!     PG18  no rows        SPG  one row per column
//!
//! PG omits materialized views from `information_schema` entirely —
//! they are not in the SQL standard. SPG's
//! `information_schema.tables` already omitted them, so the two views
//! disagreed with each other as well as with PG: a relation with
//! columns and no table row.
//!
//! Every expectation below is a PG18 reading.

use spg_engine::{Engine, QueryResult};

fn engine() -> Engine {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (a INT, b TEXT)").unwrap();
    e.execute("CREATE VIEW v AS SELECT a, b, a * 2 AS c FROM t")
        .unwrap();
    e.execute("CREATE MATERIALIZED VIEW mv AS SELECT a FROM t")
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

/// The two information_schema views agree with each other, and with PG.
#[test]
fn round536_matview_is_in_neither_information_schema_view() {
    let mut e = engine();
    assert!(
        rows(
            &mut e,
            "SELECT column_name FROM information_schema.columns WHERE table_name = 'mv'"
        )
        .is_empty(),
        "a matview has no information_schema.columns rows"
    );
    assert!(
        rows(
            &mut e,
            "SELECT table_name FROM information_schema.tables WHERE table_name = 'mv'"
        )
        .is_empty(),
        "and no information_schema.tables row"
    );
}

/// It is still a relation in pg_catalog, where PG does report it.
#[test]
fn round536_matview_is_still_in_pg_class() {
    let mut e = engine();
    assert_eq!(
        rows(
            &mut e,
            "SELECT relkind FROM pg_class WHERE relname = 'mv'"
        ),
        vec!["m"]
    );
}

/// A table and an ordinary view are unaffected — the view's computed
/// column included.
#[test]
fn round536_tables_and_views_unchanged() {
    let mut e = engine();
    assert_eq!(
        rows(
            &mut e,
            "SELECT column_name FROM information_schema.columns \
             WHERE table_name = 'v' ORDER BY ordinal_position"
        ),
        vec!["a", "b", "c"]
    );
    assert_eq!(
        rows(
            &mut e,
            "SELECT table_name, table_type FROM information_schema.tables \
             WHERE table_name IN ('t', 'v') ORDER BY table_name"
        ),
        vec!["t|BASE TABLE", "v|VIEW"]
    );
}
