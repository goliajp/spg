//! v7.39 (RLS) Phase 0 — CREATE/ALTER/DROP POLICY + ALTER TABLE {ENABLE |
//! DISABLE | FORCE | NO FORCE} ROW LEVEL SECURITY are parsed, stored, and
//! surfaced by pg_policies / pg_policy / pg_class. No enforcement yet (the
//! embedded engine runs as the Admin superuser, which PG-correctly bypasses
//! RLS). Every expected value is from live PG18.4.
//!
//! Quals use integer predicates so SPG's AST Display matches PG's deparse
//! byte-for-byte; CURRENT_USER / string-literal deparse normalisation is a
//! documented Phase-2 residual (same class as the default_text deparser tail).

use spg_engine::{Engine, QueryResult};

fn rows(e: &mut Engine, sql: &str) -> Vec<Vec<String>> {
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
                        spg_storage::Value::Bool(b) => if *b { "t" } else { "f" }.to_string(),
                        spg_storage::Value::Null => String::new(),
                        other => format!("{other:?}"),
                    })
                    .collect()
            })
            .collect(),
        other => panic!("{sql}: expected Rows, got {other:?}"),
    }
}

fn setup(e: &mut Engine) {
    e.execute("CREATE TABLE d(id int, owner text)").unwrap();
    e.execute("ALTER TABLE d ENABLE ROW LEVEL SECURITY")
        .unwrap();
    e.execute("CREATE POLICY p_sel ON d FOR SELECT USING (id > 5)")
        .unwrap();
    e.execute(
        "CREATE POLICY p_restr ON d AS RESTRICTIVE FOR UPDATE USING (id > 0) WITH CHECK (id > 10)",
    )
    .unwrap();
    e.execute("CREATE POLICY p_ins ON d FOR INSERT WITH CHECK (id >= 100)")
        .unwrap();
}

#[test]
fn pg_policies_columns_match_pg() {
    let mut e = Engine::new();
    setup(&mut e);
    let got = rows(
        &mut e,
        "SELECT policyname, permissive, roles, cmd, \
         COALESCE(qual,'') , COALESCE(with_check,'') \
         FROM pg_policies WHERE tablename='d' ORDER BY policyname",
    );
    assert_eq!(
        got,
        vec![
            vec![
                "p_ins",
                "PERMISSIVE",
                "{public}",
                "INSERT",
                "",
                "(id >= 100)"
            ],
            vec![
                "p_restr",
                "RESTRICTIVE",
                "{public}",
                "UPDATE",
                "(id > 0)",
                "(id > 10)"
            ],
            vec!["p_sel", "PERMISSIVE", "{public}", "SELECT", "(id > 5)", ""],
        ]
    );
}

#[test]
fn pg_policy_polcmd_and_permissive() {
    let mut e = Engine::new();
    setup(&mut e);
    let got = rows(
        &mut e,
        "SELECT polname, polcmd, polpermissive FROM pg_policy ORDER BY polname",
    );
    assert_eq!(
        got,
        vec![
            vec!["p_ins", "a", "t"],
            vec!["p_restr", "w", "f"],
            vec!["p_sel", "r", "t"],
        ]
    );
    // pg_get_expr(polqual, polrelid) passes the stored text through.
    let q = rows(
        &mut e,
        "SELECT pg_get_expr(polqual, polrelid) FROM pg_policy WHERE polname='p_sel'",
    );
    assert_eq!(q, vec![vec!["(id > 5)"]]);
}

#[test]
fn pg_class_flags_track_enable_force_disable() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE d(id int)").unwrap();
    let flags = |e: &mut Engine| {
        rows(
            e,
            "SELECT relrowsecurity, relforcerowsecurity FROM pg_class WHERE relname='d'",
        )
    };
    assert_eq!(flags(&mut e), vec![vec!["f", "f"]]);
    e.execute("ALTER TABLE d ENABLE ROW LEVEL SECURITY")
        .unwrap();
    assert_eq!(flags(&mut e), vec![vec!["t", "f"]]);
    e.execute("ALTER TABLE d FORCE ROW LEVEL SECURITY").unwrap();
    assert_eq!(flags(&mut e), vec![vec!["t", "t"]]);
    // DISABLE clears only relrowsecurity; force flag is independent.
    e.execute("ALTER TABLE d DISABLE ROW LEVEL SECURITY")
        .unwrap();
    assert_eq!(flags(&mut e), vec![vec!["f", "t"]]);
    e.execute("ALTER TABLE d NO FORCE ROW LEVEL SECURITY")
        .unwrap();
    assert_eq!(flags(&mut e), vec![vec!["f", "f"]]);
}

#[test]
fn clause_command_matrix_errors() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE d(id int)").unwrap();
    // FOR INSERT with USING → rejected.
    assert!(
        e.execute("CREATE POLICY e1 ON d FOR INSERT USING (id > 0)")
            .is_err()
    );
    // FOR SELECT with WITH CHECK → rejected.
    assert!(
        e.execute("CREATE POLICY e2 ON d FOR SELECT WITH CHECK (id > 0)")
            .is_err()
    );
    // FOR DELETE with WITH CHECK → rejected.
    assert!(
        e.execute("CREATE POLICY e3 ON d FOR DELETE WITH CHECK (id > 0)")
            .is_err()
    );
    // No policy landed.
    assert!(
        rows(
            &mut e,
            "SELECT policyname FROM pg_policies WHERE tablename='d'"
        )
        .is_empty()
    );
}

#[test]
fn duplicate_policy_name_errors() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE d(id int)").unwrap();
    e.execute("CREATE POLICY p ON d USING (id > 0)").unwrap();
    assert!(e.execute("CREATE POLICY p ON d USING (id > 1)").is_err());
}

#[test]
fn drop_policy_and_if_exists() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE d(id int)").unwrap();
    e.execute("CREATE POLICY p ON d USING (id > 0)").unwrap();
    // DROP missing without IF EXISTS → error.
    assert!(e.execute("DROP POLICY nope ON d").is_err());
    // IF EXISTS on missing → ok.
    e.execute("DROP POLICY IF EXISTS nope ON d").unwrap();
    // Real drop.
    e.execute("DROP POLICY p ON d").unwrap();
    assert!(
        rows(
            &mut e,
            "SELECT policyname FROM pg_policies WHERE tablename='d'"
        )
        .is_empty()
    );
}

#[test]
fn alter_policy_rename_and_replace() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE d(id int)").unwrap();
    e.execute("CREATE POLICY p ON d USING (id > 0)").unwrap();
    e.execute("ALTER POLICY p ON d RENAME TO p2").unwrap();
    e.execute("ALTER POLICY p2 ON d USING (id > 5)").unwrap();
    let got = rows(
        &mut e,
        "SELECT policyname, COALESCE(qual,'') FROM pg_policies WHERE tablename='d'",
    );
    assert_eq!(got, vec![vec!["p2", "(id > 5)"]]);
}
