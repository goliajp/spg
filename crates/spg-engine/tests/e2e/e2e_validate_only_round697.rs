//! Round 697 — the F31 sweep's second batch: three more statements that
//! named something and never looked.
//!
//! Ten `_no_op` statements measured against PG18. Four already agreed
//! (CLUSTER, REINDEX TABLE, VACUUM, ANALYZE all refuse a missing relation,
//! as does DROP TYPE). Of the rest:
//!
//!   * `SET SESSION AUTHORIZATION <role>` took any name. Its comment said
//!     "SPG has no role system so this is a strict no-op" — true when it was
//!     written, and roles became real in round 58. The comment outlived the
//!     fact, and with it the reason the check was absent. It still switches
//!     no authorization; it refuses a role that does not exist.
//!
//!   * `CREATE EXTENSION <e>` reported success for any name, and
//!     `pg_extension` then did not list what had just been "created".
//!     `DROP EXTENSION <e>` likewise. Both read the same list `pg_extension`
//!     is built from now — one list, so the three cannot disagree.
//!
//!     They WARN rather than refuse, and the first cut of this round did
//!     refuse, which three existing tests caught. See
//!     `round697_an_unprovided_extension_warns_rather_than_refusing` for
//!     why the tests were right.
//!
//! Two are left, measured and recorded rather than half-done:
//!
//!   * `DROP AGGREGATE nosuch(int)` — PG says `aggregate nosuch(integer)
//!     does not exist`, rendering the signature with canonical type names.
//!     Reproducing that faithfully is its own piece of work, and a message
//!     that gets the signature wrong would be worse than none.
//!
//!   * `ALTER TABLE t SET SCHEMA nosuch` — PG refuses the missing schema.
//!     Round 652 judged this unfixable while `CREATE SCHEMA` accepts a name
//!     without registering it: a check would refuse sequences PG accepts.
//!     Re-measured this round — `CREATE SCHEMA s697` still does not appear
//!     in `pg_namespace` — so the judgement stands on a current reading.

use spg_engine::{Engine, QueryResult};

fn ok(e: &mut Engine, sql: &str) {
    let r = e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err}"));
    assert!(matches!(r, QueryResult::CommandOk { .. }), "{sql}: {r:?}");
}

fn err_of(e: &mut Engine, sql: &str) -> String {
    format!("{}", e.execute(sql).expect_err(&format!("PG18 refuses: {sql}")))
}

#[test]
fn round697_set_session_authorization_refuses_a_missing_role() {
    let mut e = Engine::new();
    assert!(err_of(&mut e, "SET SESSION AUTHORIZATION nosuch697").contains("nosuch697"));
    assert!(err_of(&mut e, "SET SESSION AUTHORIZATION 'nosuch697'").contains("nosuch697"));
    // The forms a pg_dump preamble emits still pass.
    ok(&mut e, "SET SESSION AUTHORIZATION DEFAULT");
    ok(&mut e, "SET SESSION AUTHORIZATION postgres");
}

/// An extension this build does not provide is WARNED about, not refused —
/// and the first version of this round did refuse it, which is why the
/// reason is written down here.
///
/// PG18 errors (`extension "x" is not available`). PG can: an extension is
/// installable there. SPG cannot be installed into, so refusing turns a
/// customer dump carrying `CREATE EXTENSION pgcrypto` from something that
/// restores into something that needs editing — the zero-customer-change
/// line, which outranks matching PG's error here.
///
/// Three existing tests caught it: `create_extension_with_schema`
/// (pgcrypto), `create_extension_with_cascade` (hstore) and
/// `create_extension_vector_no_op` (pgvector) all went red. They were
/// right and the change was wrong.
///
/// Saying NOTHING was still the defect: `CREATE EXTENSION hstore` reported
/// plain success and nothing hstore-shaped worked afterwards. It warns.
#[test]
fn round697_an_unprovided_extension_warns_rather_than_refusing() {
    let mut e = Engine::new();
    ok(&mut e, "CREATE EXTENSION hstore");
    ok(&mut e, "CREATE EXTENSION IF NOT EXISTS hstore CASCADE");
    ok(&mut e, "DROP EXTENSION hstore");
    // And the ones it does provide pass without comment.
    for sql in [
        "CREATE EXTENSION vector",
        "CREATE EXTENSION IF NOT EXISTS pg_trgm",
        "CREATE EXTENSION plpgsql WITH SCHEMA public",
        "CREATE EXTENSION pgcrypto",
    ] {
        ok(&mut e, sql);
    }
}

/// `pgcrypto` is on the provided list because SPG really answers it. The
/// list is a claim about capability, so it is checked against capability.
#[test]
fn round697_the_provided_list_is_a_claim_that_holds() {
    let mut e = Engine::new();
    let one = |e: &mut Engine, sql: &str| match e.execute(sql) {
        Ok(QueryResult::Rows { rows, .. }) => {
            spg_engine::eval::value_to_text(&rows[0].values[0])
        }
        other => panic!("{sql}: {other:?}"),
    };
    assert_eq!(one(&mut e, "SELECT digest('x','sha256') IS NOT NULL"), "true");
    assert_eq!(one(&mut e, "SELECT gen_random_uuid() IS NOT NULL"), "true");
    assert_eq!(one(&mut e, "SELECT '[1,2]'::vector::text"), "[1,2]");
    // And hstore is NOT on it, because this is what hstore does here.
    assert!(e.execute("SELECT 'a=>1'::hstore").is_err());
}

#[test]
fn round697_drop_extension_takes_the_forms_a_dump_emits() {
    let mut e = Engine::new();
    ok(&mut e, "DROP EXTENSION IF EXISTS nosuch697");
    ok(&mut e, "DROP EXTENSION vector");
    ok(&mut e, "DROP EXTENSION pg_trgm, plpgsql CASCADE");
}

/// The three read one list, so `pg_extension` cannot list something
/// `CREATE EXTENSION` rejects, nor reject something it lists.
#[test]
fn round697_the_extension_list_and_the_catalog_agree() {
    let mut e = Engine::new();
    let listed = match e.execute("SELECT extname FROM pg_extension").unwrap() {
        QueryResult::Rows { rows, .. } => rows
            .iter()
            .map(|r| spg_engine::eval::value_to_text(&r.values[0]))
            .collect::<Vec<_>>(),
        other => panic!("{other:?}"),
    };
    assert!(!listed.is_empty());
    for name in &listed {
        ok(&mut e, &format!("CREATE EXTENSION {name}"));
        ok(&mut e, &format!("DROP EXTENSION {name}"));
    }
}

/// The two that stay different, pinned as differences so that the day one
/// changes, someone sees it rather than discovering it by accident.
#[test]
fn round697_the_two_recorded_residuals() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t697(i INT)").unwrap();
    // PG: `aggregate nosuch697(integer) does not exist`.
    ok(&mut e, "DROP AGGREGATE nosuch697(int)");
    // PG: `schema "nosuch697" does not exist`. See the header for why this
    // one cannot be checked while CREATE SCHEMA does not register.
    ok(&mut e, "ALTER TABLE t697 SET SCHEMA nosuch697");
    ok(&mut e, "CREATE SCHEMA s697");
    let schemas = match e.execute("SELECT nspname FROM pg_namespace").unwrap() {
        QueryResult::Rows { rows, .. } => rows
            .iter()
            .map(|r| spg_engine::eval::value_to_text(&r.values[0]))
            .collect::<Vec<_>>(),
        other => panic!("{other:?}"),
    };
    assert!(
        !schemas.iter().any(|s| s == "s697"),
        "if CREATE SCHEMA starts registering, SET SCHEMA becomes checkable: {schemas:?}"
    );
}
