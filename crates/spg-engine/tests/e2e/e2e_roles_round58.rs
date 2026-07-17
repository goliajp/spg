//! v7.39 (read01 round 58) — the role system: attributes, membership,
//! inheritance, and the introspection that reports them.
//!
//! Round 57 made table privileges real but left four residuals, and all four
//! lived here: `SET ROLE` accepted a role that did not exist, `GRANT devs TO
//! alice` was a no-op, `pg_roles` hard-coded its three attribute columns, and
//! `has_column_privilege` / `pg_has_role` answered an unconditional `true` —
//! which, now that privileges are enforced, is the same lie round 57 killed:
//! a role that cannot read the table at all would be told it can read a column.
//!
//! PG's rules, byte-locked against a live PG18.4 oracle:
//!   - CREATE USER is CREATE ROLE … LOGIN. A bare CREATE ROLE cannot log in but
//!     holds privileges and has members.
//!   - INHERIT (the default) means a member automatically holds its roles'
//!     privileges. NOINHERIT means it must SET ROLE to them.
//!   - ATTRIBUTES are never inherited — only privileges flow through membership.

use spg_engine::{Engine, QueryResult};

fn ok(e: &mut Engine, sql: &str) {
    e.execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"));
}

fn err(e: &mut Engine, sql: &str) -> String {
    format!("{}", e.execute(sql).unwrap_err())
}

fn r1(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => spg_engine::eval::value_to_text(&rows[0].values[0]),
        other => panic!("{sql}: {other:?}"),
    }
}

fn col(e: &mut Engine, sql: &str) -> Vec<String> {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => rows
            .iter()
            .map(|r| spg_engine::eval::value_to_text(&r.values[0]))
            .collect(),
        other => panic!("{sql}: {other:?}"),
    }
}

fn seeded() -> Engine {
    let mut e = Engine::new();
    // A group role: no password, cannot log in, but holds privileges.
    ok(&mut e, "CREATE ROLE devs");
    ok(&mut e, "CREATE ROLE ann LOGIN PASSWORD 'x'");
    ok(&mut e, "CREATE ROLE carl LOGIN NOINHERIT PASSWORD 'x'");
    ok(&mut e, "CREATE TABLE rt (id int, v text)");
    ok(&mut e, "INSERT INTO rt VALUES (1,'a')");
    e
}

#[test]
fn privileges_flow_through_membership_to_an_inheriting_member() {
    let mut e = seeded();
    assert_eq!(
        r1(&mut e, "SELECT has_table_privilege('ann','rt','SELECT')"),
        "false"
    );
    ok(&mut e, "GRANT devs TO ann");
    ok(&mut e, "GRANT SELECT ON rt TO devs");
    // ann INHERITs, so the group's grant is hers without any SET ROLE.
    assert_eq!(
        r1(&mut e, "SELECT has_table_privilege('ann','rt','SELECT')"),
        "true"
    );
    ok(&mut e, "SET ROLE ann");
    assert_eq!(r1(&mut e, "SELECT count(*) FROM rt"), "1");
    ok(&mut e, "RESET ROLE");
    // Revoking the membership takes the privilege away again — inheritance is
    // resolved live, not snapshotted at GRANT time.
    ok(&mut e, "REVOKE devs FROM ann");
    assert_eq!(
        r1(&mut e, "SELECT has_table_privilege('ann','rt','SELECT')"),
        "false"
    );
}

#[test]
fn a_noinherit_member_must_set_role_explicitly() {
    let mut e = seeded();
    ok(&mut e, "GRANT devs TO carl");
    ok(&mut e, "GRANT SELECT ON rt TO devs");
    // carl is a member, but NOINHERIT: the privilege does not flow to him.
    assert_eq!(
        r1(&mut e, "SELECT has_table_privilege('carl','rt','SELECT')"),
        "false"
    );
    ok(&mut e, "SET ROLE carl");
    assert_eq!(
        err(&mut e, "SELECT count(*) FROM rt"),
        "unsupported: permission denied for table rt"
    );
    // Becoming devs is what unlocks it.
    ok(&mut e, "SET ROLE devs");
    assert_eq!(r1(&mut e, "SELECT count(*) FROM rt"), "1");
}

#[test]
fn set_role_rejects_a_role_that_does_not_exist() {
    let mut e = seeded();
    // Before roles were real, any name was accepted — and a typo silently put
    // the session into a role that held nothing.
    assert_eq!(
        err(&mut e, "SET ROLE nosuchrole"),
        "unsupported: role \"nosuchrole\" does not exist"
    );
    ok(&mut e, "SET ROLE ann");
}

#[test]
fn pg_roles_reports_the_real_attributes() {
    let mut e = seeded();
    assert_eq!(
        col(
            &mut e,
            "SELECT rolname||'|'||rolcanlogin::text||'|'||rolinherit::text||'|'||rolsuper::text \
             FROM pg_roles WHERE rolname IN ('ann','carl','devs') ORDER BY rolname"
        ),
        [
            "ann|true|true|false",
            "carl|true|false|false",
            "devs|false|true|false"
        ]
    );
}

#[test]
fn pg_auth_members_joins_back_to_pg_roles() {
    let mut e = seeded();
    ok(&mut e, "GRANT devs TO ann");
    ok(&mut e, "GRANT devs TO carl");
    // The canonical join every RBAC-aware tool runs.
    assert_eq!(
        r1(
            &mut e,
            "SELECT count(*) FROM pg_auth_members m JOIN pg_roles r ON r.oid = m.roleid \
             WHERE r.rolname = 'devs'"
        ),
        "2"
    );
}

#[test]
fn pg_has_role_and_column_privilege_stop_lying() {
    let mut e = seeded();
    ok(&mut e, "GRANT devs TO ann");
    ok(&mut e, "GRANT devs TO carl");
    ok(&mut e, "GRANT SELECT ON rt TO devs");
    // MEMBER = can SET ROLE to it. USAGE = the privileges flow automatically,
    // which is the INHERIT question — so carl (NOINHERIT) is a member without
    // USAGE.
    assert_eq!(
        r1(&mut e, "SELECT pg_has_role('ann','devs','MEMBER')"),
        "true"
    );
    assert_eq!(
        r1(&mut e, "SELECT pg_has_role('carl','devs','USAGE')"),
        "false"
    );
    // SPG has no column-level grants, so a column privilege IS the table's —
    // and that is the truthful answer, not an unconditional `true`.
    assert_eq!(
        r1(
            &mut e,
            "SELECT has_column_privilege('ann','rt','v','SELECT')"
        ),
        "true"
    );
    assert_eq!(
        r1(
            &mut e,
            "SELECT has_column_privilege('carl','rt','v','SELECT')"
        ),
        "false"
    );
}

#[test]
fn a_role_holding_privileges_cannot_be_dropped() {
    let mut e = seeded();
    ok(&mut e, "GRANT SELECT ON rt TO devs");
    // PG names the objects that still depend on the role.
    let msg = err(&mut e, "DROP ROLE devs");
    assert!(
        msg.contains("cannot be dropped because some objects depend on it"),
        "{msg}"
    );
    assert!(msg.contains("privileges for table rt"), "{msg}");
    ok(&mut e, "REVOKE ALL ON rt FROM devs");
    ok(&mut e, "DROP ROLE devs");
}

#[test]
fn a_superuser_role_bypasses_every_check() {
    let mut e = seeded();
    ok(&mut e, "CREATE ROLE root LOGIN SUPERUSER PASSWORD 'x'");
    ok(&mut e, "SET ROLE root");
    // No grant anywhere, and yet: superuser.
    assert_eq!(r1(&mut e, "SELECT count(*) FROM rt"), "1");
    ok(&mut e, "DELETE FROM rt");
    // The attribute is NOT inherited through membership (PG's rule): ann is a
    // member of a superuser role but is not one herself.
    ok(&mut e, "RESET ROLE");
    ok(&mut e, "GRANT root TO ann");
    ok(&mut e, "SET ROLE ann");
    assert_eq!(
        err(&mut e, "DROP TABLE rt"),
        "unsupported: must be owner of table rt"
    );
}
