//! v7.39 (round 247) — COPY TO option sweep, 17 cases against live
//! PG18.4 (2026-07-19). The text/CSV cores, HEADER, DELIMITER, NULL,
//! QUOTE, column lists and the query form already matched byte-for-byte
//! (including quote-doubling and embedded newlines); the gaps:
//!
//!   * `FORCE_QUOTE (cols)` / `FORCE_QUOTE *` did not parse — non-NULL
//!     cells of the named columns always quote, NULLs stay bare;
//!   * `ESCAPE` did not parse — inside a quoted cell the quote (and the
//!     escape itself) take the escape prefix instead of doubling;
//!   * a TEXT-mode QUOTE / ESCAPE / FORCE_QUOTE was silently IGNORED;
//!     PG refuses each by name ("COPY QUOTE requires CSV mode", 0A000);
//!   * a multi-byte DELIMITER / QUOTE / ESCAPE takes PG's wording
//!     ("COPY delimiter must be a single one-byte character").
//!
//! Recorded divergence: FORMAT binary stays an honest refusal (PG emits
//! its binary protocol; a binary COPY is its own round). The
//! differential probe also learned quote-aware statement splitting — a
//! `;` inside a string literal (`DELIMITER ';'`) shredded the statement.

use spg_engine::{Engine, QueryResult};

fn seeded() -> Engine {
    let mut e = Engine::new();
    e.execute("CREATE TABLE ct (id int, name text, note text, f float8)")
        .unwrap();
    e.execute("INSERT INTO ct VALUES (1, 'plain', NULL, 1.5), (2, 'with,comma', 'q\"uote', 2.25)")
        .unwrap();
    e
}

fn lines(e: &mut Engine, sql: &str) -> Vec<String> {
    match e.execute(sql).unwrap_or_else(|x| panic!("{sql}: {x:?}")) {
        QueryResult::Rows { rows, .. } => rows
            .iter()
            .map(|r| spg_engine::eval::value_to_text(&r.values[0]))
            .collect(),
        other => panic!("{sql}: {other:?}"),
    }
}

fn err(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql) {
        Err(x) => format!("{x}"),
        Ok(ok) => panic!("{sql}: expected an error, got {ok:?}"),
    }
}

#[test]
fn force_quote_columns_and_star() {
    let mut e = seeded();
    assert_eq!(
        lines(&mut e, "COPY ct TO STDOUT (FORMAT csv, FORCE_QUOTE (name))"),
        ["1,\"plain\",,1.5", "2,\"with,comma\",\"q\"\"uote\",2.25"]
    );
    // `*` forces every column; NULLs stay bare (PG).
    assert_eq!(
        lines(&mut e, "COPY ct TO STDOUT (FORMAT csv, FORCE_QUOTE *)"),
        [
            "\"1\",\"plain\",,\"1.5\"",
            "\"2\",\"with,comma\",\"q\"\"uote\",\"2.25\""
        ]
    );
}

#[test]
fn escape_replaces_quote_doubling() {
    let mut e = seeded();
    assert_eq!(
        lines(&mut e, "COPY ct TO STDOUT (FORMAT csv, ESCAPE '\\')"),
        ["1,plain,,1.5", "2,\"with,comma\",\"q\\\"uote\",2.25"]
    );
}

#[test]
fn csv_only_options_are_refused_in_text_mode() {
    let mut e = seeded();
    for (sql, want) in [
        (
            "COPY ct TO STDOUT (FORMAT text, QUOTE '\"')",
            "COPY QUOTE requires CSV mode",
        ),
        (
            "COPY ct TO STDOUT (FORMAT text, ESCAPE '\\')",
            "COPY ESCAPE requires CSV mode",
        ),
        (
            "COPY ct TO STDOUT (FORMAT text, FORCE_QUOTE (name))",
            "COPY FORCE_QUOTE requires CSV mode",
        ),
        (
            "COPY ct TO STDOUT (FORMAT csv, DELIMITER 'ab')",
            "COPY delimiter must be a single one-byte character",
        ),
        (
            "COPY ct TO STDOUT (FORMAT csv, ESCAPE 'ab')",
            "COPY escape must be a single one-byte character",
        ),
    ] {
        let got = err(&mut e, sql);
        assert!(got.contains(want), "{sql}\n  want {want:?}\n  got  {got:?}");
    }
}

#[test]
fn the_copy_core_is_unchanged() {
    let mut e = seeded();
    assert_eq!(
        lines(&mut e, "COPY ct TO STDOUT (FORMAT csv)"),
        ["1,plain,,1.5", "2,\"with,comma\",\"q\"\"uote\",2.25"]
    );
    assert_eq!(
        lines(&mut e, "COPY ct TO STDOUT (FORMAT csv, DELIMITER ';')"),
        ["1;plain;;1.5", "2;with,comma;\"q\"\"uote\";2.25"]
    );
    assert_eq!(
        lines(&mut e, "COPY ct TO STDOUT (FORMAT csv, HEADER)")[0],
        "id,name,note,f"
    );
    assert_eq!(
        lines(&mut e, "COPY ct (name, id) TO STDOUT (FORMAT csv)"),
        ["plain,1", "\"with,comma\",2"]
    );
    assert_eq!(
        lines(
            &mut e,
            "COPY (SELECT id FROM ct ORDER BY id) TO STDOUT (FORMAT csv, FORCE_QUOTE *)"
        ),
        ["\"1\"", "\"2\""]
    );
}
