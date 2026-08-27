//! v7.39.2 — a column named twice is refused, which it was not.
//!
//! `CREATE TABLE t (a int, a int)` built the table.
//! `information_schema.columns` then carried TWO rows named `a`, every
//! later reference to that name was ambiguous, and a dump of it restores
//! into neither engine.
//!
//! Six places could produce such a relation and exactly one — `ALTER
//! TABLE ADD COLUMN` — refused it. Measured against PostgreSQL 18.6 and
//! MySQL 9.7.2; every message below is PG's, byte for byte, and the
//! MySQL dialect answers `Duplicate column name 'a'` with errno 1060 /
//! SQLSTATE 42S21 (pinned on the wire, where the errno lives).
//!
//! The negative controls matter as much as the refusals: PostgreSQL
//! ALLOWS a duplicate column in a plain index, so a check that refuses
//! everything with a repeated name would be wrong in a way no refusal
//! pin can see.

use spg_engine::{Engine, QueryResult};

fn run(e: &mut Engine, sql: &str) -> Result<String, String> {
    match e.execute(sql) {
        Err(err) => Err(format!("{err}")),
        Ok(QueryResult::Rows { rows, .. }) if !rows.is_empty() => {
            Ok(spg_engine::eval::value_to_text(&rows[0].values[0]))
        }
        Ok(_) => Ok("<ok>".to_string()),
    }
}

fn refuses(sql: &str, want: &str) {
    let mut e = Engine::new();
    let got = run(&mut e, sql);
    let Err(msg) = got else {
        panic!("{sql} answered {got:?} where PostgreSQL 18.6 errors");
    };
    assert!(
        msg.contains(want),
        "{sql}\n  PG says: {want}\n  this says: {msg}"
    );
}

const DUP: &str = r#"column "a" specified more than once"#;

#[test]
fn create_table_refuses_a_column_named_twice() {
    refuses("CREATE TABLE t (a int, a int)", DUP);
}

#[test]
fn an_unquoted_pair_differing_only_in_case_is_one_name_twice() {
    // Not because this compares case-insensitively — the LEXER folded
    // them before this saw them, which is PostgreSQL's rule.
    refuses("CREATE TABLE t (a int, A int)", DUP);
}

#[test]
fn a_quoted_pair_differing_in_case_is_two_columns_in_postgresql() {
    // Measured on PG 18.6: this CREATES the table. Quoting preserves
    // case there, so `"a"` and `"A"` are two columns.
    //
    // The first version of this change folded case here as well and
    // refused it — a table PostgreSQL makes. No refusal pin could see
    // that; what saw it was an ablation that did NOT bite.
    let mut e = Engine::new();
    run(&mut e, r#"CREATE TABLE t ("a" int, "A" int)"#)
        .expect("PostgreSQL 18.6 creates this table");
}

#[test]
fn a_mysql_session_does_fold_case_because_mysql_does() {
    // MySQL 9.7.2: ``(`a` int, `A` int)`` is
    // `ERROR 1060 Duplicate column name 'A'`. Its column names never
    // distinguish case, quoted or not — the opposite of PostgreSQL's
    // answer to the same shape, which is why the comparison asks the
    // dialect.
    let mut e = Engine::new();
    e.execute("SET sql_mode=''").unwrap();
    let msg = run(&mut e, "CREATE TABLE t (`a` int, `A` int)").unwrap_err();
    assert!(msg.contains("Duplicate column name"), "{msg}");
}

#[test]
fn a_primary_key_refuses_a_column_named_twice() {
    // PG has its own sentence for a constraint list.
    refuses(
        "CREATE TABLE t (a int, b int, PRIMARY KEY (a, a))",
        r#"column "a" appears twice in primary key constraint"#,
    );
}

#[test]
fn a_unique_constraint_refuses_a_column_named_twice() {
    refuses(
        "CREATE TABLE t (a int, UNIQUE (a, a))",
        r#"column "a" appears twice in unique constraint"#,
    );
}

#[test]
fn a_view_column_list_refuses_a_name_twice() {
    refuses("CREATE VIEW v (a, a) AS SELECT 1, 2", DUP);
}

#[test]
fn create_table_as_refuses_a_duplicate_output_name() {
    // Checked on the RESOLVED names: `SELECT *` does not carry them
    // until the body has run, which is where PG checks it too.
    refuses("CREATE TABLE t AS SELECT 1 AS a, 2 AS a", DUP);
}

#[test]
fn nothing_is_left_behind_by_a_refusal() {
    let mut e = Engine::new();
    assert!(e.execute("CREATE TABLE t (a int, a int)").is_err());
    // The name is free: the refusal happened before anything was made.
    e.execute("CREATE TABLE t (a int, b int)")
        .expect("the refused name must not have been taken");
}

#[test]
fn the_shapes_that_must_still_be_accepted() {
    let mut e = Engine::new();
    for sql in [
        "CREATE TABLE ok1 (a int, b int)",
        "CREATE TABLE ok2 (a int, b int, PRIMARY KEY (a, b))",
        "CREATE TABLE ok3 (a int, UNIQUE (a))",
        "CREATE VIEW okv (x, y) AS SELECT 1, 2",
        "CREATE TABLE ok4 AS SELECT 1 AS a, 2 AS b",
        // PostgreSQL 18.6 ALLOWS this one, measured. A check that
        // refused every repeated name would break it.
        "CREATE TABLE ok5 (a int)",
        "CREATE INDEX ok5ix ON ok5 (a, a)",
    ] {
        run(&mut e, sql).unwrap_or_else(|m| panic!("{sql} was refused: {m}"));
    }
}

#[test]
fn a_mysql_session_uses_mysqls_words() {
    let mut e = Engine::new();
    e.execute("SET sql_mode=''").unwrap();
    let msg = run(&mut e, "CREATE TABLE t (a int, a int)").unwrap_err();
    assert!(
        msg.contains("Duplicate column name 'a'"),
        "MySQL 9.7.2 says `Duplicate column name 'a'`, this says `{msg}`"
    );
    // And the constraint form reuses the same one there, unlike PG.
    let msg = run(&mut e, "CREATE TABLE t2 (a int, b int, PRIMARY KEY (a, a))").unwrap_err();
    assert!(msg.contains("Duplicate column name 'a'"), "{msg}");
}
