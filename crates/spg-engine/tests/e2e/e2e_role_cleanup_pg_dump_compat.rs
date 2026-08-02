//! v7.37.17 (17.6 siblings) — pg_dumpall-compat role cleanup
//! statements: REASSIGN OWNED / DROP OWNED.
//!
//! v7.39 (round 696) — these took ANY role name, invented ones included,
//! because the whole statement was swallowed by the parser's dump-noise
//! list. PG18 refuses a role that does not exist, and now so does SPG; the
//! statements still PERFORM nothing (there is no role-owner model).
//!
//! A pg_dumpall restore is unaffected: it creates the roles before emitting
//! the cleanup that names them. These tests create them for the same reason.

use spg_engine::Engine;

fn ddl(e: &mut Engine, sql: &str) {
    e.execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"));
}

fn with_roles() -> Engine {
    let mut e = Engine::new();
    for r in ["olduser", "newuser", "a", "b", "c"] {
        ddl(&mut e, &format!("CREATE ROLE {r}"));
    }
    e
}

#[test]
fn reassign_owned_by_no_op() {
    let mut e = with_roles();
    ddl(&mut e, "REASSIGN OWNED BY olduser TO newuser");
    ddl(&mut e, "REASSIGN OWNED BY a, b, c TO admin");
}

#[test]
fn drop_owned_by_no_op() {
    let mut e = with_roles();
    ddl(&mut e, "DROP OWNED BY olduser");
    ddl(&mut e, "DROP OWNED BY olduser CASCADE");
    ddl(&mut e, "DROP OWNED BY a, b, c RESTRICT");
}

/// v7.39 (round 696) — and a role that was never created is refused, which
/// is the whole point of the change. Without this the tests above would
/// pass equally well if the check were removed again.
#[test]
fn round696_an_uncreated_role_is_refused() {
    let mut e = with_roles();
    for sql in [
        "DROP OWNED BY nosuch696",
        "REASSIGN OWNED BY nosuch696 TO newuser",
        "REASSIGN OWNED BY olduser TO nosuch696",
    ] {
        let err = e.execute(sql).expect_err(sql);
        assert!(format!("{err}").contains("nosuch696"), "{sql}: {err}");
    }
}
