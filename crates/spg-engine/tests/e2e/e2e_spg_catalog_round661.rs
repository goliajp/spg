//! v7.39 (round 661) — F34: `pg_proc` claimed PostgreSQL provides 86
//! functions it does not.
//!
//! Round 653 fixed the under-reporting (338 names listed against 709
//! answered) and deliberately kept 149 dialect names OUT, because listing
//! them would tell a client that PG has a `date_format`. This is the same
//! lie in the other direction, and it predates that round: 86 names already
//! sat at `pronamespace = 11`, the `pg_catalog` oid.
//!
//! Measured, all 86 are callable — none is fiction — so deleting the rows
//! would cost real discoverability. What was wrong is only the provenance.
//! Four groups:
//!
//!   * fifty invented `pg_*` names — the sharpest of them, because the
//!     prefix itself asserts PostgreSQL origin. `pg_stat_get_idx_scan`
//!     against PG's `pg_stat_get_numscans`; `pg_start_backup`, which PG15
//!     removed.
//!   * fifteen MySQL-dialect (`ifnull`, `unix_timestamp`, `benchmark`, …)
//!   * eleven from extension families (uuid-ossp, pg_trgm, pg_prewarm)
//!   * six SQL constructs PG implements in the parser, not in pg_proc
//!     (`nullif`, `current_catalog`, `user`, …), and four `spg_*`.
//!
//! PG's own answer for what core does not provide is a different namespace —
//! that is where extension functions live — so they move to `pg_spg`.
//! A client asking "does PostgreSQL provide this?" now gets the right
//! answer; one asking "can I call it?" still finds it.

use spg_engine::{Engine, QueryResult};

fn one(e: &mut Engine, sql: &str) -> String {
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
            .collect::<Vec<_>>()
            .join(","),
        other => panic!("{sql}: {other:?}"),
    }
}

/// Nothing in `pg_catalog` may be a name PG18 lacks. This is the assertion
/// the round exists for.
#[test]
fn round661_pg_catalog_claims_only_what_pg_has() {
    let mut e = Engine::new();
    assert_eq!(
        one(
            &mut e,
            "SELECT count(*) FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace \
             WHERE n.nspname = 'pg_catalog' AND p.proname IN \
             ('ifnull','unix_timestamp','benchmark','pg_stat_get_idx_scan','pg_start_backup',\
              'spg_version','uuid_generate_v4','similarity','nullif','current_catalog')"
        ),
        "0"
    );
    // …and they are all somewhere, not deleted.
    assert_eq!(
        one(
            &mut e,
            "SELECT count(*) FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace \
             WHERE n.nspname = 'pg_spg' AND p.proname IN \
             ('ifnull','unix_timestamp','benchmark','pg_stat_get_idx_scan','pg_start_backup',\
              'spg_version','uuid_generate_v4','similarity','nullif','current_catalog')"
        ),
        "10"
    );
}

/// Moving the namespace must not move the behaviour: dispatch is by name,
/// so every one of them still answers.
#[test]
fn round661_the_moved_functions_still_answer() {
    let mut e = Engine::new();
    assert_eq!(one(&mut e, "SELECT ifnull(NULL, 1)"), "1");
    assert_eq!(one(&mut e, "SELECT ucase('ab')"), "AB");
    assert_eq!(one(&mut e, "SELECT nullif(1, 1)"), "NULL");
    assert_eq!(one(&mut e, "SELECT spg_version() IS NOT NULL"), "true");
    assert_eq!(one(&mut e, "SELECT length(uuid_generate_v4()::text)"), "36");
}

/// `pg_spg` is a registered namespace, so the join resolves rather
/// than dangling — the failure mode round 638's pin was built for.
#[test]
fn round661_the_namespace_is_registered() {
    let mut e = Engine::new();
    assert_eq!(
        one(&mut e, "SELECT nspname FROM pg_namespace ORDER BY oid"),
        "pg_catalog,public,information_schema,pg_spg"
    );
    assert_eq!(
        one(
            &mut e,
            "SELECT count(*) FROM pg_proc p LEFT JOIN pg_namespace n ON n.oid = p.pronamespace \
             WHERE n.oid IS NULL"
        ),
        "0"
    );
}
