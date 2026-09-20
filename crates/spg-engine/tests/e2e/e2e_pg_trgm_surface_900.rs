//! 9.0.0 — pg_trgm's operators were not the extension's, and its
//! trigrams were not PostgreSQL's.
//!
//! Measured against a live PostgreSQL 18.6 with the extension installed:
//!
//!   * `'x' % 'y'` answered `division by zero` (it was modulo, and any
//!     type error whose message mentioned `%` was being re-read as a zero
//!     divide). PG answers `f`.
//!   * `'cat' <-> 'hat'` errored with `<-> requires two vectors, got
//!     Some(Text) and Some(Text)` — a Rust `Debug` of an `Option` in a
//!     message a client reads. PG answers `0.85714287`.
//!   * The other eight operators and fourteen functions did not exist.
//!   * Trigram extraction was ASCII-only, so `similarity('日本語','日本')`
//!     was 0 where PG says 0.4 and `show_trgm('日本語')` was empty; and an
//!     apostrophe was a word CHARACTER, so `don't` was one word where PG
//!     makes it two.
//!
//! Every expected value below was measured on PG 18.6.

use spg_engine::{Engine, QueryResult};

fn one(e: &mut Engine, sql: &str) -> String {
    let QueryResult::Rows { rows, .. } = e
        .execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"))
    else {
        panic!("expected rows for {sql}");
    };
    spg_engine::eval::value_to_text(
        rows.first()
            .expect("one row")
            .values
            .first()
            .expect("one column"),
    )
}

fn with_pg_trgm() -> Engine {
    let mut e = Engine::new();
    e.execute("CREATE EXTENSION pg_trgm")
        .expect("CREATE EXTENSION pg_trgm");
    e
}

#[test]
fn percent_over_two_texts_is_the_similarity_test() {
    let mut e = with_pg_trgm();
    assert_eq!(one(&mut e, "SELECT ('x' % 'y')::text"), "false");
    assert_eq!(one(&mut e, "SELECT ('cat' % 'cat')::text"), "true");
    // And modulo is still modulo.
    assert_eq!(one(&mut e, "SELECT (5 % 2)::text"), "1");
    assert_eq!(one(&mut e, "SELECT (5.5 % 2)::text"), "1.5");
}

#[test]
fn a_zero_divisor_is_the_only_division_by_zero() {
    let mut e = Engine::new();
    let err = e
        .execute("SELECT 5 % 0")
        .expect_err("a zero divisor still raises");
    assert!(
        format!("{err:?}").contains("DivisionByZero"),
        "5 % 0: {err:?}"
    );
    // Without the extension `%` over two texts is an operator PG does not
    // have either — and the answer says so rather than claiming a zero
    // divide.
    let err = e.execute("SELECT 'x' % 'y'").expect_err("no such operator");
    let msg = format!("{err:?}");
    assert!(msg.contains("operator does not exist"), "'x' % 'y': {msg}");
    assert!(!msg.contains("DivisionByZero"), "'x' % 'y': {msg}");
}

#[test]
fn distance_over_two_texts_is_one_minus_the_similarity() {
    let mut e = with_pg_trgm();
    assert_eq!(one(&mut e, "SELECT ('cat' <-> 'hat')::text"), "0.85714287");
}

#[test]
fn a_refused_distance_names_the_types_it_could_not_resolve() {
    let mut e = Engine::new();
    let err = e.execute("SELECT 1 <-> 2").expect_err("no such operator");
    let msg = format!("{err:?}");
    assert!(
        msg.contains("operator does not exist: integer <-> integer"),
        "{msg}"
    );
    // The message used to print the Rust `Debug` of an `Option`.
    assert!(!msg.contains("Some("), "{msg}");
}

#[test]
fn the_eight_word_operators_answer_what_pg_answers() {
    let mut e = with_pg_trgm();
    for (sql, want) in [
        ("SELECT ('cat' <% 'hat cat')::text", "true"),
        ("SELECT ('hat cat' <% 'cat')::text", "false"),
        ("SELECT ('cat' %> 'hat cat')::text", "false"),
        ("SELECT ('hat cat' %> 'cat')::text", "true"),
        ("SELECT ('cat' <<% 'hat cat')::text", "true"),
        ("SELECT ('cat' %>> 'hat cat')::text", "true"),
        ("SELECT ('cat' <<-> 'hat cat')::text", "0"),
        ("SELECT ('hat cat' <<-> 'cat')::text", "0.4285714"),
        ("SELECT ('cat' <->> 'hat cat')::text", "0.4285714"),
        ("SELECT ('cat' <<<-> 'hat cat')::text", "0"),
        ("SELECT ('cat' <->>> 'hat cat')::text", "0.4285714"),
    ] {
        assert_eq!(one(&mut e, sql), want, "{sql}");
    }
}

#[test]
fn word_similarity_scores_the_best_extent() {
    let mut e = with_pg_trgm();
    assert_eq!(
        one(&mut e, "SELECT word_similarity('word','two words')::text"),
        "0.8"
    );
    assert_eq!(
        one(
            &mut e,
            "SELECT strict_word_similarity('word','two words')::text"
        ),
        "0.5714286"
    );
    assert_eq!(
        one(&mut e, "SELECT similarity('word','two words')::text"),
        "0.36363637"
    );
    // A gap inside the extent is paid for: counting the matched trigrams
    // over the first string's own count alone would say 0.6.
    assert_eq!(
        one(&mut e, "SELECT word_similarity('abcd','ab xcd')::text"),
        "0.4"
    );
}

#[test]
fn trigrams_reach_characters_that_are_not_ascii() {
    let mut e = with_pg_trgm();
    assert_eq!(
        one(&mut e, "SELECT similarity('日本語','日本')::text"),
        "0.4"
    );
    assert_eq!(
        one(&mut e, "SELECT show_trgm('日本語')::text"),
        "{0x8194c0,0x836e53,0x1dc363,0x1e22e9}"
    );
    // PG's array order compares the three bytes as SIGNED chars, which is
    // why the two 0x8… entries come first.
    assert_eq!(
        one(&mut e, "SELECT show_trgm('héllo')::text"),
        "{0xb67cb8,0xdc85af,\"  h\",0x4f6625,llo,\"lo \"}"
    );
}

#[test]
fn an_apostrophe_separates_two_words() {
    let mut e = with_pg_trgm();
    assert_eq!(
        one(&mut e, "SELECT show_trgm('don''t')::text"),
        "{\"  d\",\"  t\",\" do\",\" t \",don,\"on \"}"
    );
}

#[test]
fn the_threshold_is_a_setting_and_set_limit_writes_it() {
    let mut e = with_pg_trgm();
    // CREATE EXTENSION is what loads the parameters, measured on 18.6.
    assert_eq!(one(&mut e, "SHOW pg_trgm.similarity_threshold"), "0.3");
    assert_eq!(one(&mut e, "SHOW pg_trgm.word_similarity_threshold"), "0.6");
    assert_eq!(
        one(&mut e, "SHOW pg_trgm.strict_word_similarity_threshold"),
        "0.5"
    );
    assert_eq!(one(&mut e, "SELECT show_limit()::text"), "0.3");
    assert_eq!(one(&mut e, "SELECT set_limit(0.7)::text"), "0.7");
    assert_eq!(one(&mut e, "SELECT show_limit()::text"), "0.7");
    // And the operator reads it.
    assert_eq!(one(&mut e, "SELECT ('cat' % 'cot')::text"), "false");
    e.execute("SELECT set_limit(0.1)").expect("set_limit");
    assert_eq!(one(&mut e, "SELECT ('cat' % 'cot')::text"), "true");
    // `SET` is the spelling PG documents, and it drives the same read.
    e.execute("SET pg_trgm.similarity_threshold = 0.9")
        .expect("SET");
    assert_eq!(one(&mut e, "SELECT show_limit()::text"), "0.9");
}

#[test]
fn a_fractional_setting_parses() {
    // `SET x = 0.7` was a syntax error: a literal with a decimal point
    // lexes as NUMERIC and only Integer and Float were admitted, so every
    // fractional planner setting refused.
    let mut e = Engine::new();
    for (sql, show, want) in [
        ("SET seq_page_cost = 0.7", "SHOW seq_page_cost", "0.7"),
        ("SET cpu_tuple_cost = 0.02", "SHOW cpu_tuple_cost", "0.02"),
        ("SET my.thing = 1e3", "SHOW my.thing", "1e3"),
        ("SET my.thing = -0.5", "SHOW my.thing", "-0.5"),
    ] {
        e.execute(sql)
            .unwrap_or_else(|err| panic!("{sql}: {err:?}"));
        assert_eq!(one(&mut e, show), want, "{sql}");
    }
}

#[test]
fn the_catalog_lists_the_extension_only_once_it_is_installed() {
    let mut e = Engine::new();
    assert_eq!(
        one(
            &mut e,
            "SELECT count(*)::text FROM pg_proc WHERE proname = 'similarity'"
        ),
        "0"
    );
    assert_eq!(
        one(
            &mut e,
            "SELECT count(*)::text FROM pg_proc WHERE proname = 'uuid_generate_v4'"
        ),
        "0"
    );
    assert_eq!(
        one(
            &mut e,
            "SELECT count(*)::text FROM pg_operator WHERE oprname = '<%'"
        ),
        "0"
    );
    e.execute("CREATE EXTENSION pg_trgm").expect("extension");
    assert_eq!(
        one(
            &mut e,
            "SELECT count(*)::text FROM pg_proc WHERE proname = 'similarity'"
        ),
        "1"
    );
    // And it is in the schema it was installed into, not in pg_catalog.
    assert_eq!(
        one(
            &mut e,
            "SELECT pronamespace::regnamespace::text FROM pg_proc WHERE proname = 'similarity'"
        ),
        "public"
    );
    assert_eq!(
        one(
            &mut e,
            "SELECT count(*)::text FROM pg_operator WHERE oprname = '<%'"
        ),
        "1"
    );
}
