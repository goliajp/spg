//! v7.12 — full-text search e2e suite (G-CRIT-3 epic).
//!
//! v7.12.0 ships types + codec; v7.12.1 wires the lexer and
//! `to_tsvector` / `plainto_tsquery` / `to_tsquery` family. This
//! file grows across v7.12.x — every new FTS surface lands here
//! before sqllogictest corpus pull-in.

use spg_engine::Engine;
use spg_engine::eval::format_tsvector;
use spg_storage::{TsLexeme, TsQueryAst, Value};

fn eng() -> Engine {
    Engine::new()
}

fn ok(e: &mut Engine, sql: &str) {
    e.execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"));
}

fn first_value(e: &mut Engine, sql: &str) -> Value {
    let r = e
        .execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    match r {
        spg_engine::QueryResult::Rows { rows, .. } => rows
            .into_iter()
            .next()
            .map(|mut row| row.values.remove(0))
            .expect("at least one row"),
        other => panic!("expected rows, got {other:?}"),
    }
}

// --- v7.12.1: to_tsvector / config dispatch ---

#[test]
fn to_tsvector_simple_keeps_words_unstemmed() {
    let mut e = eng();
    let v = first_value(
        &mut e,
        "SELECT to_tsvector('simple', 'The Quick brown Foxes')",
    );
    let lexs = match v {
        Value::TsVector(l) => l,
        other => panic!("expected tsvector, got {other:?}"),
    };
    let words: Vec<&str> = lexs.iter().map(|l| l.word.as_str()).collect();
    // Simple config: lowercased, alphabetised, no stem, no stopword drop.
    assert_eq!(words, vec!["brown", "foxes", "quick", "the"]);
}

#[test]
fn to_tsvector_english_drops_stopwords_and_stems() {
    let mut e = eng();
    let v = first_value(
        &mut e,
        "SELECT to_tsvector('english', 'The cats are running over the lazy foxes')",
    );
    let lexs = match v {
        Value::TsVector(l) => l,
        other => panic!("expected tsvector, got {other:?}"),
    };
    let words: Vec<&str> = lexs.iter().map(|l| l.word.as_str()).collect();
    assert!(words.contains(&"cat"), "expected `cat`, got {words:?}");
    assert!(words.contains(&"run"), "expected `run`, got {words:?}");
    assert!(words.contains(&"fox"), "expected `fox`, got {words:?}");
    assert!(!words.contains(&"the"), "stopword leaked");
    assert!(!words.contains(&"are"), "stopword leaked");
}

#[test]
fn to_tsvector_null_text_is_null() {
    let mut e = eng();
    let v = first_value(&mut e, "SELECT to_tsvector('english', NULL)");
    assert!(matches!(v, Value::Null), "got {v:?}");
}

#[test]
fn to_tsvector_rejects_unsupported_config() {
    let mut e = eng();
    let err = e
        .execute("SELECT to_tsvector('spanish', 'hola mundo')")
        .expect_err("spanish must error");
    let msg = format!("{err:?}");
    assert!(
        msg.contains("not implemented") && msg.contains("spanish"),
        "expected unsupported-config error, got: {msg}"
    );
}

#[test]
fn to_tsvector_round_trips_through_cast_literal() {
    // INSERT VALUES path only takes literals; round-trip through a
    // `::tsvector` cast is the v7.12.0 shape pg_dump uses. The
    // `to_tsvector()` builder itself is exercised by the other
    // SELECT tests in this file.
    let mut e = eng();
    ok(
        &mut e,
        "CREATE TABLE m (id INT NOT NULL, v tsvector NOT NULL)",
    );
    ok(
        &mut e,
        "INSERT INTO m VALUES (1, '''cat'':2 ''run'':1'::tsvector)",
    );
    let v = first_value(&mut e, "SELECT v FROM m WHERE id = 1");
    let lexs = match v {
        Value::TsVector(l) => l,
        other => panic!("expected tsvector, got {other:?}"),
    };
    let rendered = format_tsvector(&lexs);
    assert_eq!(rendered, "'cat':2 'run':1");
}

// --- v7.12.1: plainto_tsquery / to_tsquery ---

#[test]
fn plainto_tsquery_drops_stopwords_and_ands() {
    let mut e = eng();
    let v = first_value(
        &mut e,
        "SELECT plainto_tsquery('english', 'the quick brown fox')",
    );
    let ast = match v {
        Value::TsQuery(a) => a,
        other => panic!("expected tsquery, got {other:?}"),
    };
    let rendered = spg_engine::eval::format_tsquery(&ast);
    assert_eq!(rendered, "'quick' & 'brown' & 'fox'");
}

#[test]
fn to_tsquery_stems_each_leaf() {
    let mut e = eng();
    let v = first_value(
        &mut e,
        "SELECT to_tsquery('english', 'running & jumps | !cats')",
    );
    let ast = match v {
        Value::TsQuery(a) => a,
        other => panic!("expected tsquery, got {other:?}"),
    };
    let rendered = spg_engine::eval::format_tsquery(&ast);
    assert_eq!(rendered, "'run' & 'jump' | !'cat'");
}

#[test]
fn phraseto_tsquery_keeps_order_as_phrase() {
    let mut e = eng();
    let v = first_value(
        &mut e,
        "SELECT phraseto_tsquery('english', 'running cats jump')",
    );
    let ast = match v {
        Value::TsQuery(a) => a,
        other => panic!("expected tsquery, got {other:?}"),
    };
    // Phrase composition: (run <1> cat) <1> jump.
    assert!(matches!(ast, TsQueryAst::Phrase { .. }));
}

#[test]
fn websearch_handles_quoted_phrase_and_minus() {
    let mut e = eng();
    let v = first_value(
        &mut e,
        "SELECT websearch_to_tsquery('english', '\"quick brown\" -lazy')",
    );
    let ast = match v {
        Value::TsQuery(a) => a,
        other => panic!("expected tsquery, got {other:?}"),
    };
    let rendered = spg_engine::eval::format_tsquery(&ast);
    // Phrase `quick<1>brown` AND NOT lazi (english stem of `lazy`).
    assert_eq!(rendered, "'quick' <1> 'brown' & !'lazi'");
}

// --- v7.12.1: SET default_text_search_config ---

#[test]
fn set_default_text_search_config_recognised() {
    let mut e = eng();
    ok(&mut e, "SET default_text_search_config = 'english'");
    assert_eq!(
        e.session_param("default_text_search_config"),
        Some("english")
    );
}

#[test]
fn reset_removes_session_param() {
    let mut e = eng();
    ok(&mut e, "SET default_text_search_config = 'english'");
    ok(&mut e, "RESET default_text_search_config");
    assert_eq!(e.session_param("default_text_search_config"), None);
}

#[test]
fn set_default_config_drives_implicit_to_tsvector_config() {
    let mut e = eng();
    ok(&mut e, "SET default_text_search_config = 'english'");
    // No explicit config arg — engine pulls 'english' from session.
    let v = first_value(&mut e, "SELECT to_tsvector('running cats')");
    let lexs = match v {
        Value::TsVector(l) => l,
        other => panic!("expected tsvector, got {other:?}"),
    };
    let words: Vec<&str> = lexs.iter().map(|l| l.word.as_str()).collect();
    assert!(words.contains(&"run"), "english stem missing: {words:?}");
    assert!(words.contains(&"cat"), "english stem missing: {words:?}");
}

#[test]
fn set_pg_catalog_qualified_config_accepted() {
    let mut e = eng();
    ok(
        &mut e,
        "SET default_text_search_config = 'pg_catalog.english'",
    );
    // `to_tsvector('pg_catalog.english', ...)` two-arg form too.
    let v = first_value(
        &mut e,
        "SELECT to_tsvector('pg_catalog.english', 'running')",
    );
    let lexs = match v {
        Value::TsVector(l) => l,
        other => panic!("expected tsvector, got {other:?}"),
    };
    assert_eq!(lexs[0].word, "run");
}

// --- v7.12.2: @@ match operator + ts_rank ---

fn rows(e: &mut Engine, sql: &str) -> Vec<spg_storage::Row> {
    let r = e
        .execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    match r {
        spg_engine::QueryResult::Rows { rows, .. } => rows,
        other => panic!("expected rows, got {other:?}"),
    }
}

#[test]
fn ts_match_simple_lexeme_present() {
    let mut e = eng();
    let v = first_value(
        &mut e,
        "SELECT to_tsvector('english', 'the quick brown fox') @@ \
         to_tsquery('english', 'fox')",
    );
    assert!(matches!(v, Value::Bool(true)), "got {v:?}");
}

#[test]
fn ts_match_lexeme_absent_false() {
    let mut e = eng();
    let v = first_value(
        &mut e,
        "SELECT to_tsvector('english', 'the quick brown fox') @@ \
         to_tsquery('english', 'badger')",
    );
    assert!(matches!(v, Value::Bool(false)), "got {v:?}");
}

#[test]
fn ts_match_null_propagates() {
    let mut e = eng();
    let v = first_value(
        &mut e,
        "SELECT NULL::tsvector @@ to_tsquery('english', 'fox')",
    );
    assert!(matches!(v, Value::Null), "got {v:?}");
}

#[test]
fn ts_match_reverse_ordering_also_parses() {
    let mut e = eng();
    let v = first_value(
        &mut e,
        "SELECT to_tsquery('english', 'fox') @@ \
         to_tsvector('english', 'fox jumps over')",
    );
    assert!(matches!(v, Value::Bool(true)), "got {v:?}");
}

#[test]
fn ts_match_and_or_not_3vl() {
    let mut e = eng();
    let vec = "to_tsvector('english', 'cats run fast')";
    let cases = [
        ("'cat & fast'", true),
        ("'cat & badger'", false),
        ("'cat | badger'", true),
        ("'!cat'", false),
        ("'!badger'", true),
    ];
    for (q, expected) in cases {
        let v = first_value(
            &mut e,
            &format!("SELECT {vec} @@ to_tsquery('english', {q})"),
        );
        assert!(
            matches!(v, Value::Bool(b) if b == expected),
            "{q} → expected {expected}, got {v:?}"
        );
    }
}

#[test]
fn ts_match_filters_rows_in_table() {
    let mut e = eng();
    ok(
        &mut e,
        "CREATE TABLE m (id INT NOT NULL, body TEXT NOT NULL)",
    );
    ok(&mut e, "INSERT INTO m VALUES (1, 'the quick brown fox')");
    ok(&mut e, "INSERT INTO m VALUES (2, 'lazy cats sleep')");
    ok(&mut e, "INSERT INTO m VALUES (3, 'foxes hunt rabbits')");
    let r = rows(
        &mut e,
        "SELECT id FROM m WHERE to_tsvector('english', body) @@ to_tsquery('english', 'fox')",
    );
    let ids: Vec<i32> = r
        .into_iter()
        .map(|row| match &row.values[0] {
            Value::Int(n) => *n,
            other => panic!("expected int id, got {other:?}"),
        })
        .collect();
    let mut sorted = ids.clone();
    sorted.sort_unstable();
    assert_eq!(sorted, vec![1, 3]);
}

#[test]
fn ts_rank_returns_positive_float_when_matched() {
    let mut e = eng();
    let v = first_value(
        &mut e,
        "SELECT ts_rank(to_tsvector('english', 'the quick brown fox'), \
                         to_tsquery('english', 'fox & quick'))",
    );
    match v {
        Value::Float(x) => assert!(x > 0.0, "expected positive rank, got {x}"),
        other => panic!("expected float, got {other:?}"),
    }
}

#[test]
fn ts_rank_zero_when_no_match() {
    let mut e = eng();
    let v = first_value(
        &mut e,
        "SELECT ts_rank(to_tsvector('english', 'the quick brown fox'), \
                         to_tsquery('english', 'badger'))",
    );
    match v {
        Value::Float(x) => assert_eq!(x, 0.0),
        other => panic!("expected float, got {other:?}"),
    }
}

#[test]
fn ts_rank_cd_returns_positive_when_matched() {
    let mut e = eng();
    let v = first_value(
        &mut e,
        "SELECT ts_rank_cd(to_tsvector('english', 'quick brown fox jumps'), \
                            to_tsquery('english', 'fox & quick'))",
    );
    match v {
        Value::Float(x) => assert!(x > 0.0, "expected positive rank, got {x}"),
        other => panic!("expected float, got {other:?}"),
    }
}

#[test]
fn ts_rank_orders_select_by_relevance() {
    // mailrs's exact query shape: ORDER BY ts_rank(search_vector, q) DESC.
    let mut e = eng();
    ok(
        &mut e,
        "CREATE TABLE m (id INT NOT NULL, body TEXT NOT NULL)",
    );
    ok(&mut e, "INSERT INTO m VALUES (1, 'fox')");
    ok(
        &mut e,
        "INSERT INTO m VALUES (2, 'fox quick fox brown fox')",
    );
    ok(&mut e, "INSERT INTO m VALUES (3, 'fox jumps')");
    let r = rows(
        &mut e,
        "SELECT id FROM m \
         WHERE to_tsvector('english', body) @@ to_tsquery('english', 'fox') \
         ORDER BY ts_rank(to_tsvector('english', body), to_tsquery('english', 'fox')) DESC \
         LIMIT 3",
    );
    let ids: Vec<i32> = r
        .into_iter()
        .map(|row| match &row.values[0] {
            Value::Int(n) => *n,
            other => panic!("expected int id, got {other:?}"),
        })
        .collect();
    // Row 2 has 3 occurrences of 'fox' → top.
    assert_eq!(ids[0], 2, "row 2 (3× fox) should rank first; got {ids:?}");
}

#[test]
fn ts_rank_null_input_returns_null() {
    let mut e = eng();
    let v = first_value(
        &mut e,
        "SELECT ts_rank(NULL::tsvector, to_tsquery('english', 'fox'))",
    );
    assert!(matches!(v, Value::Null), "got {v:?}");
}

// --- v7.12.0 carry-forward: cast literal round trip ---

#[test]
fn cast_literal_tsvector_still_works() {
    let mut e = eng();
    // PG single-quote escape inside a string literal is doubling
    // (`''`). The cast lexeme bodies themselves are single-quoted.
    let v = first_value(&mut e, "SELECT '''foo'':1 ''bar'':2'::tsvector");
    let lexs = match v {
        Value::TsVector(l) => l,
        other => panic!("expected tsvector, got {other:?}"),
    };
    assert_eq!(lexs.len(), 2);
    assert!(lexs.iter().any(|l| l.word == "foo"));
    assert!(lexs.iter().any(|l| l.word == "bar"));
    let _: &TsLexeme = &lexs[0]; // type re-export check
}
