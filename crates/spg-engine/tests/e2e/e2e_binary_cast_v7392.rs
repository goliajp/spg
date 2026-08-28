//! v7.39.2 — casting a binary string to a character type reads its bytes.
//!
//! MySQL's `CAST(0x41 AS CHAR)` is `A`. SPG rendered the bytea PostgreSQL's
//! way and answered the six characters `\x41`, and everything built on that
//! was wrong without saying so — measured against MySQL 9.7.2:
//!
//!   CAST(0x41 AS CHAR)              'A'   → '\x41'
//!   CAST(0x41 AS CHAR) = 'A'          1   → 0
//!   LENGTH(CAST(0x4142 AS CHAR))      2   → 6
//!   CAST(0x41 AS BINARY)            'A'   → '\x41'
//!   CONVERT(0x41, CHAR)             'A'   → column "char" does not exist
//!   CONVERT(0x41 USING utf8mb4)     'A'   → syntax error at USING
//!
//! `BINARY` stays binary — the bytes are kept, not re-read as characters —
//! which is what makes its comparison byte-wise.

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
        Err(err) => format!("ERR {err}"),
    }
}

#[test]
fn a_cast_to_a_character_type_reads_the_bytes() {
    let mut e = mysql();
    for (sql, want) in [
        ("SELECT CAST(0x41 AS CHAR)", "A"),
        ("SELECT CAST(X'41' AS CHAR)", "A"),
        ("SELECT CAST(0x4142 AS CHAR(1))", "A"),
        // The answers built on it, which is where the silence was.
        ("SELECT CAST(0x41 AS CHAR)='A'", "true"),
        ("SELECT LENGTH(CAST(0x4142 AS CHAR))", "2"),
    ] {
        assert_eq!(one(&mut e, sql), want, "{sql}");
    }
}

#[test]
fn a_cast_to_binary_keeps_the_bytes() {
    // Not re-read as characters: the value stays a binary string, which is
    // what the MySQL wire prints as `A` and what makes the comparison
    // byte-wise. HEX and LENGTH describe the BYTES, so they are the
    // assertions that can tell the two apart.
    let mut e = mysql();
    for (sql, want) in [
        ("SELECT HEX(CAST(0x00FF AS BINARY))", "00FF"),
        ("SELECT LENGTH(CAST(0x4142 AS BINARY))", "2"),
        // `(n)` truncates to n BYTES.
        ("SELECT HEX(CAST(0x4142 AS BINARY(1)))", "41"),
        ("SELECT CAST('abc' AS BINARY(2))", "ab"),
        ("SELECT CAST(0x41 AS BINARY)='A'", "true"),
        ("SELECT CAST(0x41 AS BINARY)='a'", "false"),
    ] {
        assert_eq!(one(&mut e, sql), want, "{sql}");
    }
}

#[test]
fn both_of_mysqls_convert_forms_are_casts() {
    let mut e = mysql();
    for (sql, want) in [
        ("SELECT CONVERT(0x41, CHAR)", "A"),
        ("SELECT CONVERT(123, CHAR)", "123"),
        ("SELECT CONVERT(0x41 USING utf8mb4)", "A"),
        ("SELECT CONVERT('abc' USING latin1)", "abc"),
        // `USING binary` keeps it binary, and the pair below is what
        // says so: byte-wise there, folded under a character set.
        // Measured on MySQL 9.7.2.
        ("SELECT HEX(CONVERT(0x41 USING binary))", "41"),
        ("SELECT CONVERT(0x41 USING binary)='a'", "false"),
        ("SELECT CONVERT(0x41 USING utf8mb4)='a'", "true"),
    ] {
        assert_eq!(one(&mut e, sql), want, "{sql}");
    }
    // An unknown charset is refused rather than quietly ignored — the same
    // table the introducers check against.
    assert!(
        one(&mut e, "SELECT CONVERT('a' USING nosuchset)").starts_with("ERR"),
        "an unknown charset is refused"
    );
}

#[test]
fn postgres_keeps_its_own_convert_and_its_own_bytea_text() {
    // The negative control. PG's `convert(bytea, src, dest)` takes three
    // arguments and must still resolve; and a PG session's bytea renders
    // as hex, which is the form its clients expect.
    let mut e = Engine::new();
    assert_eq!(
        one(&mut e, "SELECT convert('\\x41'::bytea, 'UTF8', 'LATIN1')"),
        "\\x41"
    );
    assert_eq!(one(&mut e, "SELECT '\\x41'::bytea::text"), "\\x41");
}
