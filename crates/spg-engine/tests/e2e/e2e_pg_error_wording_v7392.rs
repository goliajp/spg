//! v7.39.2 — two PostgreSQL error wordings the differential corpus had
//! been carrying as accepted divergences.
//!
//! `to_number('x','999')` answered SPG's own `to_number(): could not
//! parse "x"`. PostgreSQL 18.6 extracts per the format and hands the
//! leftover to `numeric_in`, which fails on the blank it is given:
//! `invalid input syntax for type numeric: " "` — one space, whatever
//! the input and whatever the format width (measured with '9', '999'
//! and '99999').
//!
//! `SELECT 1 +;` answered `syntax error at end of input` where
//! PostgreSQL names the token it stopped at, `syntax error at or near
//! ";"`, and keeps `end of input` for the same text WITHOUT a
//! semicolon. SPG's parser already drew that distinction exactly — the
//! PostgreSQL wire took the terminator off before the parser could see
//! it, so both shapes collapsed onto one message. The parser sees what
//! the client sent now; the routing helpers above it still match on the
//! trimmed form.

use spg_engine::Engine;

/// The engine's Display carries classification prefixes (`eval: `,
/// `type mismatch: `) that the wire strips; what is pinned here is the
/// sentence a client sees, which the live probe against psql confirms.
fn err(e: &mut Engine, sql: &str) -> String {
    format!("{}", e.execute(sql).unwrap_err())
        .trim_start_matches("eval: ")
        .trim_start_matches("type mismatch: ")
        .to_string()
}

#[test]
fn to_number_fails_in_postgresqls_words() {
    let mut e = Engine::new();
    for (sql, want) in [
        (
            "SELECT to_number('x','999')",
            "invalid input syntax for type numeric: \" \"",
        ),
        (
            "SELECT to_number('abc','999')",
            "invalid input syntax for type numeric: \" \"",
        ),
        (
            "SELECT to_number('','999')",
            "invalid input syntax for type numeric: \" \"",
        ),
        // The width of the format does not change the blank it reports.
        (
            "SELECT to_number('x','9')",
            "invalid input syntax for type numeric: \" \"",
        ),
        (
            "SELECT to_number('x','99999')",
            "invalid input syntax for type numeric: \" \"",
        ),
    ] {
        assert_eq!(err(&mut e, sql), want, "{sql}");
    }
    // The control: the ones that DO extract digits still answer.
    for (sql, want) in [
        ("SELECT to_number('12x','999')", "12"),
        ("SELECT to_number('1,2','9,9')", "12"),
    ] {
        let got = e.execute(sql).expect(sql);
        let spg_engine::QueryResult::Rows { rows, .. } = got else {
            panic!("{sql}")
        };
        assert_eq!(
            spg_engine::eval::value_to_text(&rows[0].values[0]),
            want,
            "{sql}"
        );
    }
}

#[test]
fn a_syntax_error_names_the_semicolon_it_stopped_at() {
    // Measured on PostgreSQL 18.6: with the terminator it names it,
    // without one it says end of input.
    assert_eq!(
        spg_sql::parser::parse_statement("SELECT 1 +;")
            .unwrap_err()
            .message,
        "syntax error at or near \";\""
    );
    assert_eq!(
        spg_sql::parser::parse_statement("SELECT 1 +")
            .unwrap_err()
            .message,
        "syntax error at end of input"
    );
    assert_eq!(
        spg_sql::parser::parse_statement("SELECT * FROM;")
            .unwrap_err()
            .message,
        "syntax error at or near \";\""
    );
}
