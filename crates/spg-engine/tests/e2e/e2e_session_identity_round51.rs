//! v7.39 (read01 commands/, round 51) — session identity + the privilege
//! introspection surface. current_user / session_user / pg_get_userbyid /
//! system_user used to be hardcoded "admin" regardless of who connected;
//! they now report the startup packet's `user`. has_table_privilege
//! validates like PG, pg_class grows relacl, and
//! information_schema.role_table_grants / .table_privileges materialise.
//!
//! Reported identity and privilege SEMANTICS stay decoupled on purpose:
//! Engine::is_superuser keys on an explicit SET ROLE, not on the login name,
//! so connecting as a non-admin user cannot silently turn the session into an
//! RLS subject. Real per-role enforcement is the RLS epic's own step.

use spg_engine::{Engine, QueryResult};

fn err(e: &mut Engine, sql: &str) -> String {
    format!("{}", e.execute(sql).unwrap_err())
}

fn ok(e: &mut Engine, sql: &str) {
    e.execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"));
}

fn row(e: &mut Engine, sql: &str) -> Vec<String> {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => rows[0]
            .values
            .iter()
            .map(spg_engine::eval::value_to_text)
            .collect(),
        other => panic!("{sql}: {other:?}"),
    }
}

#[test]
fn embedded_defaults_to_admin() {
    let mut e = Engine::new();
    // No startup packet — the embedded engine keeps the Admin login.
    assert_eq!(
        row(
            &mut e,
            "SELECT current_user, session_user, pg_get_userbyid(10)"
        ),
        vec!["admin", "admin", "admin"]
    );
}

#[test]
fn session_user_follows_the_login() {
    let mut e = Engine::new();
    e.set_session_user("unmei");
    assert_eq!(
        row(
            &mut e,
            "SELECT current_user, session_user, pg_get_userbyid(10), system_user"
        ),
        vec!["unmei", "unmei", "unmei", "trust:unmei"]
    );
}

#[test]
fn has_table_privilege_validates_like_pg() {
    let mut e = Engine::new();
    ok(&mut e, "CREATE TABLE p1(a int, b text)");
    assert_eq!(
        row(&mut e, "SELECT has_table_privilege('p1', 'SELECT')"),
        vec!["true"]
    );
    // A missing relation errors (42P01), it does not answer true.
    assert!(
        err(&mut e, "SELECT has_table_privilege('nope_tbl', 'SELECT')")
            .contains("relation \"nope_tbl\" does not exist")
    );
    // An unknown privilege word errors (22023).
    assert!(
        err(&mut e, "SELECT has_table_privilege('p1', 'BOGUS')")
            .contains("unrecognized privilege type: \"BOGUS\"")
    );
    // A "WITH GRANT OPTION" suffix is legal.
    assert_eq!(
        row(
            &mut e,
            "SELECT has_table_privilege('p1', 'SELECT WITH GRANT OPTION')"
        ),
        vec!["true"]
    );
}

#[test]
fn privilege_introspection_views() {
    let mut e = Engine::new();
    ok(&mut e, "CREATE TABLE p1(a int)");
    // relacl exists and is NULL — PG's shape when only the owner's implicit
    // privileges apply. (Round 57 made the GRANT side real: relacl materialises
    // on the first GRANT. See e2e_acl_round57.)
    assert_eq!(
        row(
            &mut e,
            "SELECT relname, relacl IS NULL FROM pg_class WHERE relname='p1'"
        ),
        vec!["p1", "true"]
    );
    // The owner's seven implicit table privileges.
    match e
        .execute(
            "SELECT privilege_type FROM information_schema.role_table_grants \
             WHERE table_name='p1' ORDER BY privilege_type",
        )
        .unwrap()
    {
        QueryResult::Rows { rows, .. } => {
            let got: Vec<String> = rows
                .iter()
                .map(|r| spg_engine::eval::value_to_text(&r.values[0]))
                .collect();
            assert_eq!(
                got,
                vec![
                    "DELETE",
                    "INSERT",
                    "REFERENCES",
                    "SELECT",
                    "TRIGGER",
                    "TRUNCATE",
                    "UPDATE"
                ]
            );
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(
        row(
            &mut e,
            "SELECT count(*) FROM information_schema.table_privileges WHERE table_name='p1'"
        ),
        vec!["7"]
    );
}
