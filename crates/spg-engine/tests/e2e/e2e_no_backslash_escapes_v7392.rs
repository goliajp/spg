//! v7.39.2 — `NO_BACKSLASH_ESCAPES` turns off escapes, not the dialect.
//!
//! "Does this session speak MySQL" and "does `\` escape inside a string"
//! were one flag. `SET sql_mode='NO_BACKSLASH_ESCAPES'` — which is what
//! you set to make a dump portable — answered the second and thereby
//! answered the first, and the session left the dialect entirely.
//! Measured on MySQL 9.7.2 against SPG before the split, nine of
//! thirteen probes diverged:
//!
//!   LENGTH('中')  3 → 1        'A' = 'a'   1 → 0
//!   'a' || 'b'    0 → 'ab'     1 AND 2     1 → error
//!   7 DIV 2       3 → syntax error         1/0   NULL → error
//!   0x41          'A' → 65     '3x' + 1    4 → error
//!   VERSION()     starts '9' → starts 'P'
//!
//! Two of those are silent wrong answers in a WHERE clause. The engine
//! asks the dialect question of its own flag now; only the lexer reads
//! the escape one.

use spg_engine::{Engine, QueryResult};

fn one(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql) {
        Ok(QueryResult::Rows { rows, .. }) if !rows.is_empty() => {
            spg_engine::eval::value_to_text(&rows[0].values[0])
        }
        Ok(_) => "<none>".to_string(),
        Err(err) => format!("ERR {err}"),
    }
}

fn mysql_without_escapes() -> Engine {
    let mut e = Engine::new();
    e.execute("SET sql_mode='NO_BACKSLASH_ESCAPES'")
        .expect("sql_mode");
    e
}

#[test]
fn the_escape_rule_really_is_off() {
    // The control. Without this the test below could pass by never
    // having left the default dialect at all.
    let mut e = mysql_without_escapes();
    assert_eq!(one(&mut e, r"SELECT LENGTH('\n')"), "2");
    let mut d = Engine::new();
    d.execute("SET sql_mode=''").expect("sql_mode");
    assert_eq!(one(&mut d, r"SELECT LENGTH('\n')"), "1");
}

#[test]
fn every_other_axis_of_the_dialect_survives_it() {
    let mut e = mysql_without_escapes();
    for (sql, want) in [
        // LENGTH counts bytes in MySQL, characters in PG.
        ("SELECT LENGTH('中')", "3"),
        // `||` is OR, and two non-numeric strings are both false. The
        // engine spells a boolean `false`; the MySQL wire spells it 0,
        // which is what a client sees and what MySQL 9.7.2 prints.
        ("SELECT 'a' || 'b'", "false"),
        ("SELECT 1 AND 2", "true"),
        ("SELECT 7 DIV 2", "3"),
        // PG raises here; MySQL answers NULL.
        ("SELECT 1/0", "NULL"),
        // Lexed as a binary string rather than a number — the engine
        // spells those PG's way, and the MySQL wire spells the same
        // bytes `A` (pinned in `e2e_mysqlwire_query.rs`).
        ("SELECT 0x41", "\\x41"),
        // The default collation folds, so this is 1 — and it is the one
        // that decides rows in a WHERE clause.
        ("SELECT 'A'='a'", "true"),
        ("SELECT '3x'+1", "4"),
        // Rendering a binary string AS TEXT is a third flag again
        // (`RenderStyle::mysql`), and it was reading the escape one
        // too. MySQL 9.7.2 answers 'A' with and without
        // NO_BACKSLASH_ESCAPES; PG's bytea text is `\x41`.
        // `CAST(0x41 AS CHAR)` is NOT here: it answers `\x41` under
        // MySQL's default sql_mode too, so it is a defect of its own
        // rather than one of this one's — recorded, not folded in.
        ("SELECT CONCAT(0x41,'')", "A"),
    ] {
        assert_eq!(one(&mut e, sql), want, "{sql}");
    }
    assert!(
        one(&mut e, "SELECT VERSION()").starts_with('9'),
        "the server still says it is MySQL"
    );
    // The default isolation level is the one thing that still goes
    // through `in_mysql_dialect()`: MySQL's default is REPEATABLE READ,
    // PostgreSQL's is READ COMMITTED.
    assert_eq!(
        one(&mut e, "SELECT @@transaction_isolation"),
        "REPEATABLE-READ"
    );
}

#[test]
fn a_postgres_session_is_untouched() {
    // The negative control: none of the above may follow a PG session,
    // which never sends sql_mode at all.
    let mut e = Engine::new();
    assert_eq!(one(&mut e, "SELECT length('中')"), "1");
    assert_eq!(one(&mut e, "SELECT 'a' || 'b'"), "ab");
    assert!(
        one(&mut e, "SELECT version()").starts_with('P'),
        "a PG session still says PostgreSQL"
    );
}

#[test]
fn leaving_the_dialect_puts_every_axis_back() {
    // `set_mysql_dialect(false)` is what a harness switching between the
    // two dialects calls — the sqllogictest runner's `dialect postgres`
    // line is the caller that found this. Entering used to be
    // `set_backslash_escapes(true)`, one axis of several; leaving has to
    // put back all of them or the next file runs half-MySQL.
    let mut e = Engine::new();
    e.set_mysql_dialect(true);
    e.execute("SET sql_mode='ANSI_QUOTES,NO_BACKSLASH_ESCAPES'")
        .expect("sql_mode");
    assert_eq!(one(&mut e, "SELECT LENGTH('中')"), "3");

    e.set_mysql_dialect(false);
    // Back to PG on every axis the sql_mode above moved: byte counting,
    // `"…"` as an identifier, and the grammar.
    assert_eq!(one(&mut e, "SELECT length('中')"), "1");
    assert_eq!(one(&mut e, "SELECT 'a' || 'b'"), "ab");
    assert!(
        one(&mut e, "SELECT \"abc\"").starts_with("ERR"),
        "`\"abc\"` is an identifier again, so it names no column"
    );

    // And back in again, from the state leaving left behind. MySQL's
    // DEFAULT sql_mode carries neither ANSI_QUOTES nor
    // NO_BACKSLASH_ESCAPES, so `"abc"` is the STRING abc here — if
    // leaving had kept the old session's ANSI_QUOTES it would be an
    // identifier again and name no column.
    e.set_mysql_dialect(true);
    assert_eq!(one(&mut e, "SELECT LENGTH('中')"), "3");
    assert_eq!(one(&mut e, "SELECT \"abc\""), "abc");
}
