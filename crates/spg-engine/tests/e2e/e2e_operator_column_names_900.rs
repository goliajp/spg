//! 9.0.0 — an operator the parser lowers onto a function is still an
//! operator, so the column it projects is `?column?`.
//!
//! Reported by sentori (§4.3). SPG lowers six operators onto functions —
//! `~` `~*` `!~` `!~*` onto `regexp_like`, `^@` onto `starts_with`, `^`
//! onto `power` — and the projected column carried the function's name,
//! which is a key an ORM reads results by.
//!
//! Every expectation below is PostgreSQL 18.6's, read off `psql`.

use spg_engine::{Engine, QueryResult};

/// The name of the single column `sql` projects.
fn column_name(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err}")) {
        QueryResult::Rows { columns, .. } => {
            assert_eq!(columns.len(), 1, "{sql}: expected one column");
            columns[0].name.clone()
        }
        other => panic!("{sql}: {other:?}"),
    }
}

#[test]
fn an_operator_lowered_onto_a_function_projects_question_column() {
    let mut e = Engine::new();
    for sql in [
        "SELECT 'a' ~ 'a'",
        "SELECT 'a' ~* 'A'",
        "SELECT 'a' !~ 'b'",
        "SELECT 'a' !~* 'B'",
        "SELECT 'ab' ^@ 'a'",
        "SELECT 2 ^ 3",
    ] {
        assert_eq!(column_name(&mut e, sql), "?column?", "{sql}");
    }
}

#[test]
fn a_written_call_still_projects_its_function() {
    let mut e = Engine::new();
    for (sql, name) in [
        ("SELECT regexp_like('a','a')", "regexp_like"),
        ("SELECT starts_with('ab','a')", "starts_with"),
        ("SELECT power(2,3)", "power"),
        ("SELECT lower('A')", "lower"),
    ] {
        assert_eq!(column_name(&mut e, sql), name, "{sql}");
    }
}

/// The name is the only thing that changed: the operators still answer.
#[test]
fn the_operators_still_answer_what_they_did() {
    let mut e = Engine::new();
    for (sql, want) in [
        ("SELECT 'a' ~ 'a'", "true"),
        ("SELECT 'a' ~* 'A'", "true"),
        ("SELECT 'a' !~ 'b'", "true"),
        ("SELECT 'ab' ^@ 'a'", "true"),
        ("SELECT 2 ^ 3", "8"),
    ] {
        let got = match e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err}")) {
            QueryResult::Rows { rows, .. } => rows
                .first()
                .and_then(|r| r.values.first())
                .map(spg_engine::eval::value_to_text)
                .unwrap_or_default(),
            other => panic!("{sql}: {other:?}"),
        };
        assert_eq!(got, want, "{sql}");
    }
}
