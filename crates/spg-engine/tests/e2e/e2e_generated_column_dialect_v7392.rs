//! v7.39.2 — a stored generated column is evaluated in the session's dialect.
//!
//! `apply_generated_stored_columns` built its own `EvalContext` with
//! `mysql_dialect: false` hard-coded, so a MySQL client's
//! `GENERATED ALWAYS AS (LENGTH(s)) STORED` was computed with PG's
//! semantics — and the answer is written to disk, not derived on read.
//! Measured on MySQL 9.7.2: `'中'` stores **3** (bytes); SPG stored **1**
//! (PG counts characters). The same expression in a plain `SELECT` was
//! already right on both, which is what made this invisible.
//!
//! The three call sites (INSERT, UPDATE, and the row-assembly free fn) are
//! covered separately below: threading only one of them would leave a
//! column whose value depends on which statement last touched the row.

use spg_engine::{Engine, QueryResult};

fn mysql() -> Engine {
    let mut e = Engine::new();
    e.execute("SET sql_mode=''")
        .expect("enter the MySQL dialect");
    e
}

fn one(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql) {
        Ok(QueryResult::Rows { rows, .. }) if !rows.is_empty() => {
            spg_engine::eval::value_to_text(&rows[0].values[0])
        }
        Ok(_) => "<none>".to_string(),
        Err(err) => panic!("{sql}: {err}"),
    }
}

#[test]
fn insert_computes_the_column_in_mysqls_dialect() {
    let mut e = mysql();
    e.execute("CREATE TABLE g (s VARCHAR(10), n INT GENERATED ALWAYS AS (LENGTH(s)) STORED)")
        .expect("create");
    e.execute("INSERT INTO g (s) VALUES ('中')")
        .expect("insert");
    // MySQL 9.7.2: 3. PG: 1.
    assert_eq!(one(&mut e, "SELECT n FROM g"), "3");
}

#[test]
fn update_recomputes_the_column_in_mysqls_dialect() {
    let mut e = mysql();
    e.execute("CREATE TABLE g (s VARCHAR(10), n INT GENERATED ALWAYS AS (LENGTH(s)) STORED)")
        .expect("create");
    e.execute("INSERT INTO g (s) VALUES ('a')").expect("insert");
    e.execute("UPDATE g SET s = '中中'").expect("update");
    // Two three-byte characters.
    assert_eq!(one(&mut e, "SELECT n FROM g"), "6");
}

#[test]
fn the_pg_dialect_still_counts_characters() {
    // The negative control: the same DDL and the same row on the PG side
    // must keep PG's answer, or the fix has simply moved the wrong answer
    // to the other dialect.
    let mut e = Engine::new();
    e.execute("CREATE TABLE g (s varchar(10), n int GENERATED ALWAYS AS (length(s)) STORED)")
        .expect("create");
    e.execute("INSERT INTO g (s) VALUES ('中')")
        .expect("insert");
    assert_eq!(one(&mut e, "SELECT n FROM g"), "1");
}

#[test]
fn a_dialect_free_expression_is_unchanged_on_both_sides() {
    // CHAR_LENGTH counts characters in both engines, so this column must
    // read 1 whichever dialect wrote it. It is the control that says the
    // divergence above is about the dialect and not about the generated
    // column machinery.
    for (mut e, what) in [(mysql(), "mysql"), (Engine::new(), "pg")] {
        e.execute(
            "CREATE TABLE g (s varchar(10), n int GENERATED ALWAYS AS (CHAR_LENGTH(s)) STORED)",
        )
        .unwrap_or_else(|err| panic!("{what}: {err}"));
        e.execute("INSERT INTO g (s) VALUES ('中')")
            .unwrap_or_else(|err| panic!("{what}: {err}"));
        assert_eq!(one(&mut e, "SELECT n FROM g"), "1", "{what}");
    }
}

#[test]
fn a_before_trigger_rewrite_is_recomputed_in_mysqls_dialect() {
    // The fourth threading site: after a BEFORE INSERT trigger rewrites NEW,
    // the stored column is computed again over the trigger's output. Nothing
    // else in this file reaches it — reverting that one site alone left every
    // other test green, which is how this shape got its own pin.
    let mut e = mysql();
    e.execute("CREATE TABLE g (s VARCHAR(10), n INT GENERATED ALWAYS AS (LENGTH(s)) STORED)")
        .expect("create");
    e.execute(
        "CREATE FUNCTION widen() RETURNS trigger AS $$ BEGIN \
         NEW.s := '中'; RETURN NEW; END $$ LANGUAGE plpgsql",
    )
    .expect("function");
    e.execute("CREATE TRIGGER wtrg BEFORE INSERT ON g FOR EACH ROW EXECUTE FUNCTION widen()")
        .expect("trigger");
    // The row arrives as a one-byte 'a'; the trigger replaces it with a
    // three-byte character, so the recompute must see 3, not 1.
    e.execute("INSERT INTO g (s) VALUES ('a')").expect("insert");
    assert_eq!(one(&mut e, "SELECT n FROM g"), "3");
}
