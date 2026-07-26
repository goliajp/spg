//! v7.39 (round 522) — the session survives a system-catalog scan.
//!
//! A SELECT whose FROM names a system view runs against a temp engine
//! holding the materialised catalog, and that engine inherited nothing
//! of the session. So every session-scoped answer changed the moment a
//! catalog appeared in the FROM clause — measured on a live server:
//!
//!     SELECT current_user                    -> unmei
//!     SELECT current_user FROM pg_class …    -> admin
//!     SET work_mem = '8MB';
//!     SELECT current_setting('work_mem')     -> 8MB
//!     … FROM pg_settings …                   -> 4MB   (the boot default)
//!
//! Nothing errored, which is what makes it worth a test: a privilege
//! check written against a catalog join was reading a different identity
//! than the same check written without one.
//!
//! The assertions here are not literal values but the invariant PG has —
//! the two shapes agree — so they hold whatever the session's user and
//! settings happen to be.

use spg_engine::{Engine, QueryResult};

fn engine() -> Engine {
    let mut e = Engine::new();
    e.execute("CREATE TABLE u1 (a INT)").unwrap();
    e
}

fn text(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err}")) {
        QueryResult::Rows { rows, .. } => rows
            .first()
            .map(|r| {
                r.values
                    .iter()
                    .map(spg_engine::eval::value_to_text)
                    .collect::<Vec<_>>()
                    .join("|")
            })
            .unwrap_or_default(),
        other => panic!("{sql}: {other:?}"),
    }
}

/// A parameter set in this session reads the same with a catalog in the
/// FROM clause as without one.
#[test]
fn round522_set_value_survives_a_catalog_scan() {
    let mut e = engine();
    e.execute("SET work_mem = '8MB'").unwrap();
    let bare = text(&mut e, "SELECT current_setting('work_mem')");
    assert_eq!(bare, "8MB");
    for view in ["pg_class", "pg_settings", "pg_roles"] {
        assert_eq!(
            text(
                &mut e,
                &format!("SELECT current_setting('work_mem') FROM {view} LIMIT 1")
            ),
            bare,
            "current_setting over {view}"
        );
    }
}

/// The session's identity is the same one either way. This is the case
/// that mattered: the temp engine fell back to its own default role.
#[test]
fn round522_session_identity_survives_a_catalog_scan() {
    let mut e = engine();
    let bare = text(&mut e, "SELECT current_user");
    assert!(!bare.is_empty());
    for view in ["pg_class", "pg_settings"] {
        assert_eq!(
            text(&mut e, &format!("SELECT current_user FROM {view} LIMIT 1")),
            bare,
            "current_user over {view}"
        );
    }
}

/// A custom `SET app.x` — the shape request-context and RLS policies are
/// written with — reads the same on both sides.
#[test]
fn round522_custom_guc_survives_a_catalog_scan() {
    let mut e = engine();
    e.execute("SET app.tenant = 'acme'").unwrap();
    assert_eq!(text(&mut e, "SELECT current_setting('app.tenant')"), "acme");
    assert_eq!(
        text(
            &mut e,
            "SELECT current_setting('app.tenant') FROM pg_class LIMIT 1"
        ),
        "acme"
    );
}

/// `application_name` read as empty over a catalog, which is not "unset"
/// — it is a value, and monitoring queries join on it.
#[test]
fn round522_application_name_survives_a_catalog_scan() {
    let mut e = engine();
    e.execute("SET application_name = 'reporter'").unwrap();
    assert_eq!(
        text(
            &mut e,
            "SELECT current_setting('application_name') FROM pg_settings LIMIT 1"
        ),
        "reporter"
    );
}
