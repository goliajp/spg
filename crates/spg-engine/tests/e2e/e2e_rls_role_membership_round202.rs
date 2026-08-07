//! v7.39 (read01 round 202) — an RLS policy `TO grp` applies to
//! transitive MEMBERS of grp (PG role inheritance). Live-PG18
//! differential 2026-07-18: member sees the policy-granted row;
//! pre-r202 SPG default-denied (fail-closed, but PG grants).

use spg_engine::{Engine, QueryResult};

fn ids(e: &mut Engine, sql: &str) -> Vec<i32> {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => rows
            .into_iter()
            .map(|r| match r.values[0] {
                spg_storage::Value::Int(n) => n,
                ref o => panic!("{o:?}"),
            })
            .collect(),
        other => panic!("{other:?}"),
    }
}

fn setup(e: &mut Engine) {
    e.execute("CREATE ROLE grp").unwrap();
    e.execute("CREATE ROLE app1").unwrap();
    e.execute("GRANT grp TO app1").unwrap();
    e.execute("CREATE TABLE rt (id INT, owner TEXT)").unwrap();
    e.execute("INSERT INTO rt VALUES (1,'app1'),(2,'other')")
        .unwrap();
    e.execute("GRANT SELECT ON rt TO app1").unwrap();
    e.execute("GRANT SELECT ON rt TO grp").unwrap();
    e.execute("ALTER TABLE rt ENABLE ROW LEVEL SECURITY")
        .unwrap();
    e.execute("CREATE POLICY p1 ON rt FOR SELECT TO grp USING (owner = 'app1')")
        .unwrap();
}

#[test]
fn member_inherits_group_policy() {
    let mut e = Engine::new();
    setup(&mut e);
    e.execute("SET ROLE app1").unwrap();
    assert_eq!(
        ids(&mut e, "SELECT id FROM rt ORDER BY id"),
        [1],
        "member of grp sees the grp policy's rows (PG inheritance)"
    );
    e.execute("RESET ROLE").unwrap();
}

#[test]
fn nested_grant_inherits() {
    let mut e = Engine::new();
    setup(&mut e);
    e.execute("CREATE ROLE app2").unwrap();
    e.execute("CREATE ROLE mid").unwrap();
    e.execute("GRANT grp TO mid").unwrap();
    e.execute("GRANT mid TO app2").unwrap();
    e.execute("GRANT SELECT ON rt TO app2").unwrap();
    e.execute("SET ROLE app2").unwrap();
    assert_eq!(
        ids(&mut e, "SELECT id FROM rt ORDER BY id"),
        [1],
        "app2 -> mid -> grp transitive inheritance"
    );
    e.execute("RESET ROLE").unwrap();
}

#[test]
fn non_member_still_denied() {
    let mut e = Engine::new();
    setup(&mut e);
    e.execute("CREATE ROLE outsider").unwrap();
    e.execute("GRANT SELECT ON rt TO outsider").unwrap();
    e.execute("SET ROLE outsider").unwrap();
    assert!(
        ids(&mut e, "SELECT id FROM rt").is_empty(),
        "non-member: no applicable permissive policy, default-deny"
    );
    e.execute("RESET ROLE").unwrap();
}
