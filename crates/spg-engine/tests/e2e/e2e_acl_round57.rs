//! v7.39 (read01 round 57) — table privileges for real: GRANT / REVOKE,
//! `pg_class.relacl`, `has_table_privilege`, and enforcement.
//!
//! Until this round GRANT and REVOKE were swallowed as pg_dump noise, `relacl`
//! was hard-NULL and `has_table_privilege` hard-`true` — each excused in a
//! comment by "SPG is single-user". That excuse died with the RLS epic, which
//! gave sessions a real role. Introspection that always answers "yes" is worse
//! than no introspection: it lies about security.
//!
//! Enforcement keys on the RLS superuser rule — the default login and an
//! explicit `SET ROLE admin` bypass everything, so a session that never assumes
//! another role sees no change. Every expectation is byte-locked against a live
//! PG18.4 oracle.

use spg_engine::{Engine, QueryResult};

fn ok(e: &mut Engine, sql: &str) {
    e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err:?}"));
}

fn err(e: &mut Engine, sql: &str) -> String {
    alloc_fmt(e.execute(sql).unwrap_err())
}

fn alloc_fmt(err: spg_engine::EngineError) -> String {
    // Display (not Debug), so quotes are not backslash-escaped.
    format!("{err}")
}

fn r1(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => spg_engine::eval::value_to_text(&rows[0].values[0]),
        other => panic!("{sql}: {other:?}"),
    }
}

fn seeded() -> Engine {
    let mut e = Engine::new();
    ok(&mut e, "CREATE USER bob WITH PASSWORD 'x' ROLE 'readwrite'");
    ok(&mut e, "CREATE TABLE ac (id int, v text)");
    ok(&mut e, "CREATE TABLE other (id int)");
    ok(&mut e, "INSERT INTO ac VALUES (1,'a')");
    ok(&mut e, "INSERT INTO other VALUES (1)");
    e
}

fn relacl(e: &mut Engine) -> String {
    r1(
        e,
        "SELECT coalesce(relacl, 'NULL') FROM pg_class WHERE relname='ac'",
    )
}

#[test]
fn relacl_is_null_until_the_first_grant_then_materialises() {
    let mut e = seeded();
    // PG leaves relacl NULL while only the owner's implicit privileges apply…
    assert_eq!(relacl(&mut e), "NULL");
    // …and materialises the WHOLE list — the owner's own default entry
    // included — the moment the first GRANT lands.
    ok(&mut e, "GRANT SELECT ON ac TO bob");
    assert_eq!(relacl(&mut e), "{admin=arwdDxtm/admin,bob=r/admin}");
    ok(&mut e, "GRANT ALL ON ac TO bob");
    assert_eq!(relacl(&mut e), "{admin=arwdDxtm/admin,bob=arwdDxtm/admin}");
    ok(&mut e, "REVOKE INSERT ON ac FROM bob");
    assert_eq!(relacl(&mut e), "{admin=arwdDxtm/admin,bob=rwdDxtm/admin}");
    // PUBLIC is the EMPTY grantee — `=r/owner`.
    ok(&mut e, "GRANT SELECT ON ac TO PUBLIC");
    assert_eq!(
        relacl(&mut e),
        "{admin=arwdDxtm/admin,bob=rwdDxtm/admin,=r/admin}"
    );
    // Revoking everything drops the grantees' entries but NOT the owner's:
    // once relacl is materialised it stays. It does not go back to NULL.
    ok(&mut e, "REVOKE ALL ON ac FROM bob");
    ok(&mut e, "REVOKE ALL ON ac FROM PUBLIC");
    assert_eq!(relacl(&mut e), "{admin=arwdDxtm/admin}");
    // WITH GRANT OPTION renders as a trailing `*` on the privilege letter.
    ok(&mut e, "GRANT SELECT ON ac TO bob WITH GRANT OPTION");
    assert_eq!(relacl(&mut e), "{admin=arwdDxtm/admin,bob=r*/admin}");
}

#[test]
fn has_table_privilege_answers_from_the_acl() {
    let mut e = seeded();
    assert_eq!(r1(&mut e, "SELECT has_table_privilege('bob','ac','SELECT')"), "false");
    ok(&mut e, "GRANT SELECT ON ac TO bob");
    assert_eq!(r1(&mut e, "SELECT has_table_privilege('bob','ac','SELECT')"), "true");
    assert_eq!(r1(&mut e, "SELECT has_table_privilege('bob','ac','INSERT')"), "false");
    // The owner holds everything implicitly, ACL or no ACL.
    assert_eq!(r1(&mut e, "SELECT has_table_privilege('admin','ac','DELETE')"), "true");
}

#[test]
fn a_policy_subject_role_is_held_to_the_acl() {
    let mut e = seeded();
    ok(&mut e, "GRANT SELECT ON ac TO bob");
    ok(&mut e, "SET ROLE bob");
    // The one privilege bob holds works…
    assert_eq!(r1(&mut e, "SELECT count(*) FROM ac"), "1");
    // …and every other one is PG's message, verbatim.
    assert_eq!(
        err(&mut e, "INSERT INTO ac VALUES (2,'b')"),
        "unsupported: permission denied for table ac"
    );
    assert_eq!(
        err(&mut e, "UPDATE ac SET v='x' WHERE id=1"),
        "unsupported: permission denied for table ac"
    );
    assert_eq!(
        err(&mut e, "DELETE FROM ac"),
        "unsupported: permission denied for table ac"
    );
    // Reshaping or destroying a table takes OWNERSHIP, not a privilege.
    assert_eq!(
        err(&mut e, "DROP TABLE ac"),
        "unsupported: must be owner of table ac"
    );
}

#[test]
fn a_subquery_is_not_a_way_around_the_gate() {
    // The read path has four public entries and two of them short-circuit below
    // the common core, so a check that only lives on the "normal" path is not a
    // check. bob holds nothing on `other`.
    let mut e = seeded();
    ok(&mut e, "GRANT SELECT ON ac TO bob");
    ok(&mut e, "SET ROLE bob");
    for sql in [
        "SELECT count(*) FROM other",
        "SELECT (SELECT count(*) FROM other)",
        "SELECT 1 WHERE 1 IN (SELECT id FROM other)",
        "SELECT count(*) FROM ac a JOIN other o ON a.id = o.id",
    ] {
        assert_eq!(
            err(&mut e, sql),
            "unsupported: permission denied for table other",
            "{sql} must not read a table bob was never granted"
        );
    }
}

#[test]
fn update_needs_select_only_once_it_reads() {
    let mut e = seeded();
    ok(&mut e, "GRANT UPDATE ON ac TO bob");
    ok(&mut e, "SET ROLE bob");
    // No WHERE, constant right-hand side: UPDATE alone is enough (PG agrees).
    ok(&mut e, "UPDATE ac SET v='x'");
    // A WHERE reads column values, so it takes SELECT too…
    assert_eq!(
        err(&mut e, "UPDATE ac SET v='y' WHERE id=1"),
        "unsupported: permission denied for table ac"
    );
    // …and so does an assignment whose right-hand side reads a column.
    assert_eq!(
        err(&mut e, "UPDATE ac SET v=v||'z'"),
        "unsupported: permission denied for table ac"
    );
    ok(&mut e, "RESET ROLE");
    ok(&mut e, "GRANT SELECT ON ac TO bob");
    ok(&mut e, "SET ROLE bob");
    ok(&mut e, "UPDATE ac SET v='y' WHERE id=1");
}

#[test]
fn grant_validates_its_role_and_its_relation() {
    let mut e = seeded();
    assert_eq!(
        err(&mut e, "GRANT SELECT ON ac TO nosuchrole"),
        "unsupported: role \"nosuchrole\" does not exist"
    );
    assert_eq!(
        err(&mut e, "GRANT SELECT ON nosuchtbl TO bob"),
        "unsupported: relation \"nosuchtbl\" does not exist"
    );
    assert_eq!(
        err(&mut e, "GRANT BOGUS ON ac TO bob"),
        "unsupported: unrecognized privilege type: \"BOGUS\""
    );
}

#[test]
fn information_schema_reports_the_grants() {
    let mut e = seeded();
    ok(&mut e, "GRANT SELECT ON ac TO bob WITH GRANT OPTION");
    ok(&mut e, "GRANT UPDATE ON ac TO bob");
    let rows = match e
        .execute(
            "SELECT grantor||'|'||grantee||'|'||privilege_type||'|'||is_grantable \
             FROM information_schema.table_privileges \
             WHERE table_name='ac' AND grantee='bob' ORDER BY privilege_type",
        )
        .unwrap()
    {
        QueryResult::Rows { rows, .. } => rows
            .iter()
            .map(|r| spg_engine::eval::value_to_text(&r.values[0]))
            .collect::<Vec<_>>(),
        other => panic!("{other:?}"),
    };
    assert_eq!(rows, ["admin|bob|SELECT|YES", "admin|bob|UPDATE|NO"]);
}

#[test]
fn a_grant_on_any_other_object_class_still_restores() {
    // pg_dump grants on schemas, sequences and functions. SPG enforces TABLE
    // privileges only; the rest parse and no-op so a dump loads cleanly.
    let mut e = seeded();
    for sql in [
        "GRANT USAGE ON SCHEMA public TO bob",
        "GRANT ALL ON SEQUENCE some_seq TO bob",
        "GRANT EXECUTE ON FUNCTION f(int) TO bob",
        "GRANT ALL ON DATABASE app TO bob",
        "GRANT SELECT ON ALL TABLES IN SCHEMA public TO bob",
        "REVOKE ALL ON SCHEMA public FROM PUBLIC",
    ] {
        ok(&mut e, sql);
    }
    // None of them touched the table ACL.
    assert_eq!(relacl(&mut e), "NULL");
}

#[test]
fn a_superuser_session_is_untouched() {
    // The default login never assumes a role, so nothing above applies to it —
    // this is the shape every existing customer runs in.
    let mut e = seeded();
    ok(&mut e, "GRANT SELECT ON ac TO bob");
    ok(&mut e, "SET ROLE bob");
    assert!(e.execute("DELETE FROM ac").is_err());
    ok(&mut e, "RESET ROLE");
    ok(&mut e, "DELETE FROM ac");
    ok(&mut e, "DROP TABLE ac");
}
