//! v7.40.11 — a keyword is a legal alias, and every one of them was a
//! syntax error.
//!
//! Reported against 7.40.9 for `release` and `savepoint`: two
//! statements in a shipped subcommand of the reporter's could not be
//! parsed at all. It is the whole unreserved class, not those two.
//!
//! The build accepted them everywhere EXCEPT the alias slot:
//!
//! ```text
//!   CREATE TABLE rk (release int)   accepted
//!   SELECT release FROM rk          accepted
//!   SELECT r.release FROM rk r      accepted
//!   SELECT 1 AS release             syntax error at or near "release"
//!   SELECT 1 release                syntax error at or near "release"
//!   SELECT * FROM rk AS release     syntax error at or near "release"
//! ```
//!
//! `expect_ident_like` has known the unreserved class since v7.17;
//! `parse_optional_alias` checked for `Token::Ident` before calling it,
//! so no keyword token ever reached the function written to accept them.
//!
//! Measured on PostgreSQL 18.6, which is what these expectations are:
//!
//! ```text
//!   SELECT 1 AS release / savepoint / show / index / limit / between   1
//!   SELECT 1 release                                                   1
//!   SELECT 1 limit             syntax error at end of input
//! ```
//!
//! So after `AS` every keyword is a label there, and bare, only the
//! unreserved ones are — `limit` bare is the clause. Both halves are
//! pinned, including the second one, because accepting `limit` as a
//! bare alias would turn `SELECT 1 limit 2` into nonsense.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn one(eng: &mut Engine, sql: &str) -> Vec<Vec<Value<'static>>> {
    match eng
        .execute(sql)
        .unwrap_or_else(|e| panic!("{sql:?}: {e:?}"))
    {
        QueryResult::Rows { rows, .. } => rows.into_iter().map(|r| r.values).collect(),
        other => panic!("{sql:?}: expected Rows, got {other:?}"),
    }
}

#[test]
fn the_two_that_were_reported() {
    let mut eng = Engine::new();
    assert_eq!(
        one(&mut eng, "SELECT 1 AS release"),
        vec![vec![Value::Int(1)]]
    );
    assert_eq!(
        one(&mut eng, "SELECT 1 AS savepoint"),
        vec![vec![Value::Int(1)]]
    );
}

/// The rest of the class, because fixing the two that were reported and
/// leaving the others is how this pattern keeps coming back.
#[test]
fn the_whole_unreserved_class_after_as() {
    let mut eng = Engine::new();
    for kw in [
        "release",
        "savepoint",
        "show",
        "index",
        "begin",
        "commit",
        "rollback",
        "drop",
        "insert",
        "partition",
        "tables",
        "extract",
    ] {
        let sql = format!("SELECT 1 AS {kw}");
        assert_eq!(
            one(&mut eng, &sql),
            vec![vec![Value::Int(1)]],
            "{kw} after AS"
        );
    }
}

/// The column NAME reports the alias, not `?column?` — an alias the
/// parser accepted but dropped would pass the row assertion above.
#[test]
fn the_alias_reaches_the_column_name() {
    let mut eng = Engine::new();
    match eng.execute("SELECT 1 AS release").expect("parses") {
        QueryResult::Rows { columns, .. } => {
            assert_eq!(columns[0].name, "release");
        }
        other => panic!("{other:?}"),
    }
}

/// Without `AS`, which PG also takes.
#[test]
fn bare_keyword_aliases_work_too() {
    let mut eng = Engine::new();
    match eng.execute("SELECT 1 release").expect("parses") {
        QueryResult::Rows { columns, rows } => {
            assert_eq!(columns[0].name, "release");
            assert_eq!(rows[0].values[0], Value::Int(1));
        }
        other => panic!("{other:?}"),
    }
}

/// A table alias, which is the third position the reporter's statements
/// could have used.
#[test]
fn a_table_alias_can_be_a_keyword() {
    let mut eng = Engine::new();
    eng.execute("CREATE TABLE kk (a INT)").unwrap();
    eng.execute("INSERT INTO kk VALUES (7)").unwrap();
    assert_eq!(
        one(&mut eng, "SELECT release.a FROM kk AS release"),
        vec![vec![Value::Int(7)]]
    );
}

/// And the half that must NOT change: a keyword that begins a trailing
/// clause is the clause, not a label. PG reserves `limit` for exactly
/// this reason — `SELECT 1 limit` is `syntax error at end of input`
/// there — and swallowing it as an alias here would turn
/// `SELECT 1 limit 2` into a two-token nonsense.
#[test]
fn a_clause_keyword_is_still_the_clause() {
    let mut eng = Engine::new();
    eng.execute("CREATE TABLE lm (g INT)").unwrap();
    eng.execute("INSERT INTO lm VALUES (1),(2),(3),(4),(5)")
        .unwrap();
    // The shape that actually reaches the alias slot: nothing between
    // the select item and the keyword. The first draft of this test
    // wrote `SELECT g FROM lm ORDER BY g LIMIT 2`, where `FROM` follows
    // the item and `LIMIT` is never in alias position — the ablation
    // that removes the exclusion did not bite, which is what said so.
    assert_eq!(
        one(&mut eng, "SELECT 1 LIMIT 2"),
        vec![vec![Value::Int(1)]],
        "LIMIT directly after the select item is still the clause"
    );
    assert_eq!(
        one(&mut eng, "SELECT g FROM lm ORDER BY g LIMIT 2").len(),
        2,
        "and with a FROM in between"
    );
    assert_eq!(
        one(&mut eng, "SELECT g FROM lm ORDER BY g LIMIT 2 OFFSET 1")
            .into_iter()
            .map(|r| r[0].clone())
            .collect::<Vec<_>>(),
        vec![Value::Int(2), Value::Int(3)],
        "OFFSET too"
    );
}

/// v7.40.11 — `<alias>.*` as a function ARGUMENT is the whole row.
///
/// Reported against 7.40.9. Every spelling was `syntax error at or near
/// "*"` while the bare alias — the same value — worked:
///
/// ```text
///                        PG 18.6              SPG 7.40.9
///   to_jsonb(t.*)     {"a": 1, "b": "x"}   syntax error at or near "*"
///   row_to_json(t.*)  {"a":1,"b":"x"}      same
///   pg_column_size(t.*) 30                 same
///   count(t.*)        1                    same
///   to_jsonb(t)       {"a": 1, "b": "x"}   {"a": 1, "b": "x"}
/// ```
///
/// The reporter uses the bare-alias form, so it cost them nothing; they
/// filed it because a user porting SQL would hit it.
#[test]
fn a_whole_row_reference_can_be_spelled_with_a_star() {
    let mut eng = Engine::new();
    eng.execute("CREATE TABLE wr (a INT, b TEXT)").unwrap();
    eng.execute("INSERT INTO wr VALUES (1, 'x')").unwrap();

    let starred = one(&mut eng, "SELECT to_jsonb(t.*)::text FROM wr t");
    let bare = one(&mut eng, "SELECT to_jsonb(t)::text FROM wr t");
    assert_eq!(starred, bare, "the two spellings are one value");
    assert_eq!(
        starred,
        vec![vec![Value::text("{\"a\": 1, \"b\": \"x\"}".to_string())]],
        "and it is what PG 18.6 answers"
    );

    assert_eq!(
        one(&mut eng, "SELECT row_to_json(t.*)::text FROM wr t"),
        vec![vec![Value::text("{\"a\":1,\"b\":\"x\"}".to_string())]]
    );
    assert_eq!(
        one(&mut eng, "SELECT count(t.*) FROM wr t"),
        vec![vec![Value::BigInt(1)]]
    );
}

/// And a select item's own `t.*` still expands to the column list —
/// the two meanings live in different positions and must stay apart.
#[test]
fn a_select_items_star_still_expands() {
    let mut eng = Engine::new();
    eng.execute("CREATE TABLE wr2 (a INT, b TEXT)").unwrap();
    eng.execute("INSERT INTO wr2 VALUES (1, 'x')").unwrap();
    let rows = one(&mut eng, "SELECT t.* FROM wr2 t");
    assert_eq!(
        rows,
        vec![vec![Value::Int(1), Value::text("x".to_string())]],
        "two columns, not one composite"
    );
}
