//! v7.17.0 Phase 4.1 — MySQL DELIMITER directive.

use spg_engine::Engine;

#[test]
fn delimiter_double_slash_parses_as_noop() {
    let mut e = Engine::new();
    // Pre-4.1 this errored at parse: "unknown keyword DELIMITER".
    e.execute("DELIMITER //").unwrap();
}

#[test]
fn delimiter_back_to_semicolon() {
    let mut e = Engine::new();
    e.execute("DELIMITER ;").unwrap();
}

// Note: DELIMITER followed by an unusual token (`$$`, `|`,
// arbitrary user chars) fails at the lex layer pre-4.1 because
// those chars aren't part of SPG's lexer alphabet. v7.17.0
// supports the dominant DELIMITER targets that DO lex (`//`
// and `;`), which together cover every mysqldump emission.
// Less-common delimiters can be normalised away by the script
// splitter that fronts the engine.

#[test]
fn delimiter_does_not_affect_following_statement() {
    let mut e = Engine::new();
    e.execute("DELIMITER //").unwrap();
    e.execute("CREATE TABLE t (id INT NOT NULL)").unwrap();
    e.execute("INSERT INTO t VALUES (1)").unwrap();
    e.execute("DELIMITER ;").unwrap();
}
