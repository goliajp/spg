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

/// Two columns as "a|b" per row (NULL → empty).
fn pair(e: &mut Engine, sql: &str) -> Vec<String> {
    match e
        .execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"))
    {
        QueryResult::Rows { rows, .. } => rows
            .iter()
            .map(|r| {
                r.values
                    .iter()
                    .map(|v| match v {
                        spg_storage::Value::Text(s) => s.to_string(),
                        spg_storage::Value::Int(n) => n.to_string(),
                        spg_storage::Value::BigInt(n) => n.to_string(),
                        spg_storage::Value::Null => String::new(),
                        other => format!("{other:?}"),
                    })
                    .collect::<Vec<_>>()
                    .join("|")
            })
            .collect(),
        other => panic!("{sql}: expected Rows, got {other:?}"),
    }
}

#[test]
fn join_filters_rls_table_via_security_barrier() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE doc(id int, owner text)").unwrap();
    e.execute("INSERT INTO doc VALUES (1,'alice'),(2,'bob'),(3,'alice')")
        .unwrap();
    e.execute("CREATE TABLE tag(doc_id int, label text)")
        .unwrap();
    e.execute("INSERT INTO tag VALUES (1,'a'),(2,'b'),(3,'c')")
        .unwrap();
    e.execute("ALTER TABLE doc ENABLE ROW LEVEL SECURITY")
        .unwrap();
    e.execute("CREATE POLICY p ON doc FOR SELECT USING (owner = current_user)")
        .unwrap();
    // Superuser join sees everything.
    assert_eq!(
        pair(
            &mut e,
            "SELECT d.id, t.label FROM doc d JOIN tag t ON d.id=t.doc_id ORDER BY d.id"
        ),
        vec!["1|a", "2|b", "3|c"]
    );
    e.execute("SET ROLE alice").unwrap();
    // INNER JOIN: doc filtered to alice (rows 1,3) before the join.
    assert_eq!(
        pair(
            &mut e,
            "SELECT d.id, t.label FROM doc d JOIN tag t ON d.id=t.doc_id ORDER BY d.id"
        ),
        vec!["1|a", "3|c"]
    );
    // LEFT JOIN with the RLS table on the nullable side: bob's doc (id=2) is
    // hidden, so tag row 2 gets a NULL owner (the barrier filters pre-join).
    assert_eq!(
        pair(
            &mut e,
            "SELECT t.doc_id, d.owner FROM tag t LEFT JOIN doc d ON d.id=t.doc_id ORDER BY t.doc_id"
        ),
        vec!["1|alice", "2|", "3|alice"]
    );
}

/// Rows affected by a CommandOk statement.
fn affected(e: &mut Engine, sql: &str) -> usize {
    match e
        .execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"))
    {
        QueryResult::CommandOk { affected, .. } => affected,
        other => panic!("{sql}: expected CommandOk, got {other:?}"),
    }
}

#[test]
fn insert_with_check_enforced() {
    let mut e = Engine::new();
    // Policy has USING; its WITH CHECK falls back to USING (PG semantics).
    e.execute("CREATE TABLE doc(id int, owner text)").unwrap();
    e.execute("ALTER TABLE doc ENABLE ROW LEVEL SECURITY")
        .unwrap();
    e.execute("CREATE POLICY p ON doc USING (owner = current_user)")
        .unwrap();
    e.execute("SET ROLE alice").unwrap();
    // Own row → allowed; another owner → "new row violates ...".
    e.execute("INSERT INTO doc VALUES (1,'alice')").unwrap();
    assert!(e.execute("INSERT INTO doc VALUES (2,'bob')").is_err());
    assert_eq!(
        col(&mut e, "SELECT id::text FROM doc ORDER BY id"),
        vec!["1"]
    );
}

#[test]
fn insert_denied_when_no_policy() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t(id int)").unwrap();
    e.execute("ALTER TABLE t ENABLE ROW LEVEL SECURITY")
        .unwrap();
    e.execute("SET ROLE alice").unwrap();
    // RLS on, no applicable policy → every new row violates.
    assert!(e.execute("INSERT INTO t VALUES (1)").is_err());
}

#[test]
fn update_using_visibility_and_with_check() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE doc(id int, owner text)").unwrap();
    e.execute("INSERT INTO doc VALUES (1,'alice'),(2,'bob'),(3,'alice')")
        .unwrap();
    e.execute("ALTER TABLE doc ENABLE ROW LEVEL SECURITY")
        .unwrap();
    e.execute(
        "CREATE POLICY p ON doc USING (owner = current_user) WITH CHECK (owner = current_user)",
    )
    .unwrap();
    e.execute("SET ROLE alice").unwrap();
    // Update own row, keep owner → 1 row.
    assert_eq!(affected(&mut e, "UPDATE doc SET id=99 WHERE id=1"), 1);
    // Flip owner to bob → WITH CHECK violation.
    assert!(e.execute("UPDATE doc SET owner='bob' WHERE id=3").is_err());
    // Update a row hidden by USING (bob's) → silently 0, no error.
    assert_eq!(affected(&mut e, "UPDATE doc SET id=88 WHERE id=2"), 0);
}

#[test]
fn delete_using_visibility() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE doc(id int, owner text)").unwrap();
    e.execute("INSERT INTO doc VALUES (1,'alice'),(2,'bob')")
        .unwrap();
    e.execute("ALTER TABLE doc ENABLE ROW LEVEL SECURITY")
        .unwrap();
    e.execute("CREATE POLICY p ON doc USING (owner = current_user)")
        .unwrap();
    e.execute("SET ROLE alice").unwrap();
    // Delete own row → 1; delete a hidden (bob's) row → 0.
    assert_eq!(affected(&mut e, "DELETE FROM doc WHERE id=1"), 1);
    assert_eq!(affected(&mut e, "DELETE FROM doc WHERE id=2"), 0);
    // Bob's row survives (superuser view).
    e.execute("RESET ROLE").unwrap();
    assert_eq!(
        col(&mut e, "SELECT owner FROM doc ORDER BY id"),
        vec!["bob"]
    );
}

#[test]
fn superuser_writes_bypass() {
    let mut e = Engine::new();
    owned_docs(&mut e);
    // Default session writes any owner freely (bypass).
    e.execute("INSERT INTO doc VALUES (9,'anyone')").unwrap();
    assert_eq!(affected(&mut e, "UPDATE doc SET owner='x' WHERE id=2"), 1);
}
