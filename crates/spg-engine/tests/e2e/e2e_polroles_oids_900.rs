//! 9.0.0 — `pg_policy.polroles` said every policy applies to PUBLIC.
//!
//! The column is `oid[]`, and every entry was 0 — which is PUBLIC on
//! PostgreSQL. So `CREATE POLICY p ON t FOR SELECT TO alice` read
//! `{0}`, and anything reading the raw catalog was told the policy
//! applies to everyone.
//!
//! `pg_policies.roles`, the VIEW over the same fact, named `alice`
//! correctly all along — two surfaces answering one question, and only
//! one of them right.

use spg_engine::{Engine, QueryResult};

fn col(e: &mut Engine, sql: &str) -> Vec<String> {
    let QueryResult::Rows { rows, .. } = e
        .execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"))
    else {
        panic!("expected rows for {sql}");
    };
    rows.into_iter()
        .map(|r| spg_engine::eval::value_to_text(r.values.first().expect("one column")))
        .collect()
}

fn seeded() -> Engine {
    let mut e = Engine::new();
    for sql in [
        "CREATE USER alice WITH PASSWORD 'x'",
        "CREATE TABLE pt (id int, owner text)",
        "ALTER TABLE pt ENABLE ROW LEVEL SECURITY",
        "CREATE POLICY p1 ON pt FOR SELECT TO alice USING (true)",
        "CREATE POLICY p2 ON pt FOR SELECT USING (true)",
    ] {
        e.execute(sql)
            .unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    }
    e
}

#[test]
fn a_named_grantee_carries_its_own_oid() {
    let mut e = seeded();
    // The oid resolves to the role, which is the whole point of the
    // column; the NUMBER is SPG's own and need not equal PG's.
    assert_eq!(
        col(
            &mut e,
            "SELECT p.polname||' -> '||coalesce(r.rolname,'(none)') \
             FROM pg_policy p LEFT JOIN pg_roles r ON r.oid = p.polroles[1] ORDER BY 1"
        ),
        vec!["p1 -> alice".to_string(), "p2 -> (none)".to_string()]
    );
}

#[test]
fn public_is_still_zero() {
    let mut e = seeded();
    // PostgreSQL writes `{0}` for a policy with no role list, and `0`
    // names no role.
    assert_eq!(
        col(
            &mut e,
            "SELECT polroles::text FROM pg_policy WHERE polname='p2'"
        ),
        vec!["{0}".to_string()]
    );
    assert_ne!(
        col(
            &mut e,
            "SELECT polroles::text FROM pg_policy WHERE polname='p1'"
        ),
        vec!["{0}".to_string()]
    );
}

/// `TO PUBLIC` written out, which the parser may keep as a role name
/// rather than an empty list. Measured on PostgreSQL 18.6: `{0}`.
#[test]
fn public_written_out_is_zero_too() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE pt (id int)").unwrap();
    e.execute("ALTER TABLE pt ENABLE ROW LEVEL SECURITY")
        .unwrap();
    e.execute("CREATE POLICY p3 ON pt FOR SELECT TO PUBLIC USING (true)")
        .unwrap();
    assert_eq!(
        col(
            &mut e,
            "SELECT polroles::text FROM pg_policy WHERE polname='p3'"
        ),
        vec!["{0}".to_string()]
    );
}

#[test]
fn the_two_surfaces_agree() {
    let mut e = seeded();
    assert_eq!(
        col(
            &mut e,
            "SELECT policyname||' '||roles::text FROM pg_policies ORDER BY 1"
        ),
        vec!["p1 {alice}".to_string(), "p2 {public}".to_string()]
    );
}
