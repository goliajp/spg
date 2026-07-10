//! v7.39 (RLS) Phase 1 — SELECT `USING` enforcement gated on a non-superuser
//! `SET ROLE`. The default Admin session bypasses RLS (PG-correct for a
//! superuser); a `SET ROLE <role>` makes the session policy-subject.
//! Every expected value is from live PG18.4 (a matching CREATE ROLE + SET ROLE
//! session).

use spg_engine::{Engine, QueryResult};

/// First column of every row, as text.
fn col(e: &mut Engine, sql: &str) -> Vec<String> {
    match e
        .execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"))
    {
        QueryResult::Rows { rows, .. } => rows
            .iter()
            .map(|r| match &r.values[0] {
                spg_storage::Value::Text(s) => s.to_string(),
                spg_storage::Value::Int(n) => n.to_string(),
                spg_storage::Value::BigInt(n) => n.to_string(),
                other => format!("{other:?}"),
            })
            .collect(),
        other => panic!("{sql}: expected Rows, got {other:?}"),
    }
}

fn scalar(e: &mut Engine, sql: &str) -> String {
    col(e, sql).into_iter().next().unwrap_or_default()
}

fn owned_docs(e: &mut Engine) {
    e.execute("CREATE TABLE doc(id int, owner text)").unwrap();
    e.execute("INSERT INTO doc VALUES (1,'alice'),(2,'bob'),(3,'alice')")
        .unwrap();
    e.execute("ALTER TABLE doc ENABLE ROW LEVEL SECURITY")
        .unwrap();
    e.execute("CREATE POLICY p ON doc USING (owner = current_user)")
        .unwrap();
}

#[test]
fn superuser_session_bypasses_rls() {
    let mut e = Engine::new();
    owned_docs(&mut e);
    // Default (Admin) session sees every row — superuser bypass.
    assert_eq!(
        col(&mut e, "SELECT owner FROM doc ORDER BY id"),
        vec!["alice", "bob", "alice"]
    );
}

#[test]
fn set_role_activates_and_reset_restores() {
    let mut e = Engine::new();
    owned_docs(&mut e);
    e.execute("SET ROLE alice").unwrap();
    // current_user follows SET ROLE; session_user stays the login.
    assert_eq!(scalar(&mut e, "SELECT current_user"), "alice");
    assert_eq!(scalar(&mut e, "SELECT session_user"), "admin");
    // RLS now filters to alice's rows.
    assert_eq!(
        col(&mut e, "SELECT id::text FROM doc ORDER BY id"),
        vec!["1", "3"]
    );
    // RESET ROLE returns to the superuser session → all rows.
    e.execute("RESET ROLE").unwrap();
    assert_eq!(scalar(&mut e, "SELECT current_user"), "admin");
    assert_eq!(
        col(&mut e, "SELECT id::text FROM doc ORDER BY id"),
        vec!["1", "2", "3"]
    );
}

#[test]
fn default_deny_when_no_policy_applies() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t(id int)").unwrap();
    e.execute("INSERT INTO t VALUES (1),(2)").unwrap();
    e.execute("ALTER TABLE t ENABLE ROW LEVEL SECURITY")
        .unwrap();
    e.execute("SET ROLE alice").unwrap();
    // RLS on, no policy → deny all.
    assert_eq!(scalar(&mut e, "SELECT count(*)::text FROM t"), "0");
}

#[test]
fn permissive_or_restrictive_and() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE d(id int, owner text, secret bool)")
        .unwrap();
    e.execute("INSERT INTO d VALUES (1,'alice',false),(2,'alice',true),(3,'bob',false)")
        .unwrap();
    e.execute("ALTER TABLE d ENABLE ROW LEVEL SECURITY")
        .unwrap();
    e.execute(
        "CREATE POLICY perm_owner ON d AS PERMISSIVE FOR SELECT USING (owner = current_user)",
    )
    .unwrap();
    e.execute("CREATE POLICY perm_bob ON d AS PERMISSIVE FOR SELECT USING (owner = 'bob')")
        .unwrap();
    e.execute("CREATE POLICY restr ON d AS RESTRICTIVE FOR SELECT USING (secret = false)")
        .unwrap();
    e.execute("SET ROLE alice").unwrap();
    // (owner=alice OR owner=bob) AND secret=false → rows 1, 3.
    assert_eq!(
        col(&mut e, "SELECT id::text FROM d ORDER BY id"),
        vec!["1", "3"]
    );
}

#[test]
fn role_scoped_policy() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t(id int)").unwrap();
    e.execute("INSERT INTO t VALUES (1),(2),(3)").unwrap();
    e.execute("ALTER TABLE t ENABLE ROW LEVEL SECURITY")
        .unwrap();
    e.execute("CREATE POLICY only_bob ON t FOR SELECT TO bob USING (true)")
        .unwrap();
    // alice: no applicable policy → deny.
    e.execute("SET ROLE alice").unwrap();
    assert_eq!(scalar(&mut e, "SELECT count(*)::text FROM t"), "0");
    // bob: policy applies → all rows.
    e.execute("SET ROLE bob").unwrap();
    assert_eq!(scalar(&mut e, "SELECT count(*)::text FROM t"), "3");
}

#[test]
fn join_on_rls_table_fails_closed() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE doc(id int, owner text)").unwrap();
    e.execute("INSERT INTO doc VALUES (1,'alice')").unwrap();
    e.execute("CREATE TABLE tag(doc_id int, label text)")
        .unwrap();
    e.execute("INSERT INTO tag VALUES (1,'x')").unwrap();
    e.execute("ALTER TABLE doc ENABLE ROW LEVEL SECURITY")
        .unwrap();
    e.execute("CREATE POLICY p ON doc USING (owner = current_user)")
        .unwrap();
    // Superuser join is fine (bypass).
    e.execute("SELECT d.id FROM doc d JOIN tag t ON d.id = t.doc_id")
        .unwrap();
    // Non-superuser join touching an RLS table fails closed (no silent leak).
    e.execute("SET ROLE alice").unwrap();
    assert!(
        e.execute("SELECT d.id FROM doc d JOIN tag t ON d.id = t.doc_id")
            .is_err()
    );
}

#[test]
fn writes_fail_closed_under_non_superuser() {
    let mut e = Engine::new();
    owned_docs(&mut e);
    e.execute("SET ROLE alice").unwrap();
    // Phase-1 write-side is fail-closed (WITH CHECK / USING enforcement is
    // Phase 2): a policy-subject session cannot write an RLS table yet.
    assert!(e.execute("INSERT INTO doc VALUES (9,'alice')").is_err());
    assert!(
        e.execute("UPDATE doc SET owner='alice' WHERE id=1")
            .is_err()
    );
    assert!(e.execute("DELETE FROM doc WHERE id=1").is_err());
    // The superuser session still writes freely (bypass).
    e.execute("RESET ROLE").unwrap();
    e.execute("INSERT INTO doc VALUES (9,'x')").unwrap();
}
