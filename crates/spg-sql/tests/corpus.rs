//! v0.2 acceptance corpus: 20+ PG SELECT queries that must parse + survive a
//! Display ↔ parse round-trip with structurally equal ASTs.

use spg_sql::parser::parse_statement;

const CORPUS: &[&str] = &[
    "SELECT 1",
    "SELECT 'hello'",
    "SELECT NULL",
    "SELECT TRUE",
    "SELECT FALSE",
    "SELECT 1, 2, 3",
    "SELECT 1.5 + 2.5",
    "SELECT -42 * 2",
    "SELECT 'it''s' AS msg",
    "SELECT * FROM users",
    "SELECT * FROM accounts AS a",
    "SELECT name, email FROM users WHERE id = 1",
    "SELECT * FROM orders WHERE total > 100 AND status = 'paid'",
    "SELECT id FROM users WHERE NOT active OR last_seen < 0",
    "SELECT t.id, t.value FROM things AS t WHERE t.value <> 0",
    "SELECT a + b * c FROM nums WHERE (a > 0) AND (b < 10)",
    "SELECT 1 + 2 * (3 - 4)",
    "SELECT 'hi' AS greeting, 1 + 2 AS sum",
    "SELECT * FROM t WHERE x = TRUE OR x = FALSE",
    "SELECT u.name AS who, u.score AS pts FROM users AS u WHERE u.score >= 80",
    "SELECT 1 / 2 + 3 * 4 - 5",
];

#[test]
fn corpus_size_meets_v0_2_target() {
    assert!(
        CORPUS.len() >= 15,
        "v0.2 requires ≥15 corpus entries, got {}",
        CORPUS.len()
    );
}

#[test]
fn corpus_all_parse_successfully() {
    for q in CORPUS {
        match parse_statement(q) {
            Ok(_) => {}
            Err(e) => panic!("failed to parse {q:?}: {e}"),
        }
    }
}

#[test]
fn corpus_display_parse_round_trip_is_structural_identity() {
    for q in CORPUS {
        let original = parse_statement(q).unwrap_or_else(|e| panic!("parse {q:?}: {e}"));
        let pretty = original.to_string();
        let again = parse_statement(&pretty).unwrap_or_else(|e| {
            panic!("re-parse failed:\n  orig:  {q}\n  pretty: {pretty}\n  err:   {e}")
        });
        assert_eq!(
            original, again,
            "round-trip mismatch:\n  orig:   {q}\n  pretty: {pretty}"
        );
    }
}

/// Double round-trip — once round-tripped, the pretty output should be a fixed
/// point: pretty(parse(pretty(parse(q)))) == pretty(parse(q)).
#[test]
fn corpus_pretty_output_is_fixed_point() {
    for q in CORPUS {
        let first = parse_statement(q).unwrap().to_string();
        let second = parse_statement(&first).unwrap().to_string();
        assert_eq!(
            first, second,
            "pretty output not a fixed point for {q:?}:\n  first:  {first}\n  second: {second}"
        );
    }
}
