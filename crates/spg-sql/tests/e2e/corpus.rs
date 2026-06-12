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
    // --- v0.3: DDL & DML --------------------------------------------------
    "CREATE TABLE foo (a INT)",
    "CREATE TABLE bar (a INT NOT NULL)",
    "CREATE TABLE users (id BIGINT NOT NULL, name TEXT NOT NULL, age INT, score FLOAT)",
    "CREATE TABLE flags (id INT NOT NULL, active BOOL NOT NULL)",
    "CREATE TABLE accounts (id BIGINT NOT NULL, balance FLOAT, label TEXT NOT NULL)",
    "INSERT INTO foo VALUES (1)",
    "INSERT INTO foo VALUES (-1)",
    "INSERT INTO foo VALUES (1, 'hi', 3.14, TRUE, NULL)",
    "INSERT INTO accounts VALUES (42, NULL, 'alice')",
    "INSERT INTO t VALUES ('it''s', 1 + 2)",
    "INSERT INTO logs VALUES (NULL, 'event', FALSE)",
    "INSERT INTO foo VALUES (1, 2, 3, 4, 5, 6, 7, 8, 9, 10)",
    // --- v7.30.1 (mailrs round-24): clauses the WAL round trip used
    // to drop. Every entry here was once rendered WITHOUT the clause
    // in question, so crash replay changed statement semantics.
    "INSERT INTO t (a, b) VALUES ('x', 'y') ON CONFLICT DO NOTHING",
    "INSERT INTO t (a, b) VALUES ('x', 'y') ON CONFLICT (a, b) DO NOTHING",
    "INSERT INTO t (a, b) VALUES ('x', 'y') ON CONFLICT (a) DO UPDATE SET b = 'z'",
    "INSERT INTO t (a, b) VALUES ('x', 'y') ON CONFLICT (a) DO UPDATE SET b = excluded.b WHERE t.a <> 'frozen'",
    "INSERT INTO t (a) VALUES ('x') RETURNING id",
    "INSERT INTO t (a) SELECT a FROM src WHERE a <> '' ON CONFLICT DO NOTHING",
    "UPDATE t SET a = 'x' WHERE id = 1 RETURNING id, a",
    "DELETE FROM t WHERE id = 1 RETURNING id",
    "WITH recent AS (SELECT id FROM msgs WHERE ts > 5) SELECT * FROM recent",
    "WITH RECURSIVE walk (n) AS (SELECT 1) SELECT n FROM walk",
    "WITH a AS (SELECT 1 AS x), b AS (SELECT 2 AS y) SELECT x FROM a",
    "SELECT id FROM msgs ORDER BY score DESC FETCH FIRST 5 ROWS WITH TIES",
    "SELECT id FROM msgs ORDER BY score DESC OFFSET 10 FETCH FIRST 5 ROWS WITH TIES",
    "SELECT kind, COUNT(*) FROM msgs GROUP BY ALL",
    "SELECT v FROM UNNEST(ARRAY['a', 'b']) AS u",
    "SELECT n FROM generate_series(1, 10) AS n",
    "SELECT n FROM generate_series(1, 10, 2) AS n",
    "SELECT x FROM t AS o INNER JOIN LATERAL (SELECT o.id AS x) AS l ON TRUE",
    "SELECT LAG(v) IGNORE NULLS OVER (ORDER BY ts) FROM samples",
    "CREATE TABLE pk (id INT PRIMARY KEY, name TEXT NOT NULL)",
    "CREATE TABLE ints (n INT UNSIGNED NOT NULL)",
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
