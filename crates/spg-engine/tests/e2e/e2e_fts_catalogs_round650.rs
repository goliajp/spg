//! v7.39 (round 650) — F21: the checklist said `ts_lexize` was missing.
//! It was present and answering wrongly, which is worse.
//!
//! `ts_lexize` picked its dictionary with `dict.contains("english")`, so
//! every name SPG does not implement behaved like `simple`:
//! `ts_lexize('french_stem', 'renards')` answered `{renards}` where PG
//! stems it to `{renard}`, and `ts_lexize('nosuchdict', 'x')` answered
//! `{x}` where PG raises 42704. A silent wrong answer for every
//! dictionary in PG's catalog that SPG has not got — and the item that
//! called it absent would never have found that.
//!
//! The accepted set is measured, not assumed. PG takes `simple` and
//! `english_stem`, case-insensitively, with an optional `pg_catalog.`
//! qualifier, and REFUSES `english`: that is a configuration name, not a
//! dictionary. SPG has exactly those two — its own error says so
//! (`supported: simple, english`).
//!
//! Four of the five text-search catalogs are published with what SPG
//! actually has: two configurations, two dictionaries, one parser, two
//! templates. PG ships thirty of the first two; listing thirty would
//! claim support the engine does not have, which is the lesson round 639
//! paid for on `pg_type`. `EMPTY_PG_CATALOGS`'s own comment had already
//! called this out — the `pg_ts_*` family "would NOT be empty … stubbing
//! those empty would be a lie, so they are recorded as work" — and this
//! is that work.
//!
//! `pg_ts_config_map` is deliberately neither published nor stubbed: it
//! maps token types to dictionaries and SPG has no token-type model,
//! which is the same gap that leaves `ts_token_type` and `ts_debug`
//! unbuilt. Those three are one piece of work, not three.
//!
//! Registering a catalog took FOUR lists — the synth dispatch, the
//! parser's resolvable names, `CATALOG_RELATIONS`, and `regclass`'s own
//! hand-kept subset. Each was found by a failing probe, one at a time.

use spg_engine::{Engine, QueryResult};

fn rows(e: &mut Engine, sql: &str) -> Vec<String> {
    match e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err}")) {
        QueryResult::Rows { rows, .. } => rows
            .iter()
            .map(|r| {
                r.values
                    .iter()
                    .map(spg_engine::eval::value_to_text)
                    .collect::<Vec<_>>()
                    .join("|")
            })
            .collect(),
        other => panic!("{sql}: {other:?}"),
    }
}

fn one(e: &mut Engine, sql: &str) -> String {
    rows(e, sql).join(",")
}

#[test]
fn round650_ts_lexize_resolves_a_real_dictionary() {
    let mut e = Engine::new();
    assert_eq!(one(&mut e, "SELECT ts_lexize('english_stem', 'running')"), "{run}");
    assert_eq!(one(&mut e, "SELECT ts_lexize('simple', 'Running')"), "{running}");
    // Case-insensitive, and the schema qualifier is optional.
    assert_eq!(one(&mut e, "SELECT ts_lexize('ENGLISH_STEM', 'running')"), "{run}");
    assert_eq!(
        one(&mut e, "SELECT ts_lexize('pg_catalog.english_stem', 'running')"),
        "{run}"
    );
    // A stopword lexizes to the empty array under the stemmer, and to
    // itself under simple — measured both ways on PG.
    assert_eq!(one(&mut e, "SELECT ts_lexize('english_stem', 'the')"), "{}");
    assert_eq!(one(&mut e, "SELECT ts_lexize('simple', 'the')"), "{the}");
}

#[test]
fn round650_an_unimplemented_dictionary_is_refused_not_echoed() {
    let mut e = Engine::new();
    // Each of these used to come back as the lowercased input, because
    // the dictionary was chosen by `name.contains("english")`.
    for dict in ["french_stem", "german_stem", "russian_stem", "nosuchdict"] {
        let err = e
            .execute(&format!("SELECT ts_lexize('{dict}', 'renards')"))
            .unwrap_err();
        assert!(
            err.to_string()
                .contains(&format!("text search dictionary \"{dict}\" does not exist")),
            "{dict}: {err}"
        );
    }
}

#[test]
fn round650_the_refusal_uses_pgs_words() {
    let mut e = Engine::new();
    let err = e
        .execute("SELECT ts_lexize('french_stem', 'renards')")
        .expect_err("SPG has no french stemmer; echoing the input is the bug");
    assert!(
        err.to_string()
            .contains("text search dictionary \"french_stem\" does not exist"),
        "unexpected message: {err}"
    );
    // `english` is a CONFIGURATION, and PG refuses it here too.
    let err = e
        .execute("SELECT ts_lexize('english', 'running')")
        .expect_err("english is a config, not a dictionary");
    assert!(err.to_string().contains("does not exist"), "{err}");
}

#[test]
fn round650_the_text_search_catalogs_list_what_spg_has() {
    let mut e = Engine::new();
    assert_eq!(
        rows(&mut e, "SELECT cfgname FROM pg_ts_config ORDER BY cfgname"),
        vec!["english", "simple"]
    );
    assert_eq!(
        rows(&mut e, "SELECT dictname FROM pg_ts_dict ORDER BY dictname"),
        vec!["english_stem", "simple"]
    );
    assert_eq!(rows(&mut e, "SELECT prsname FROM pg_ts_parser"), vec!["default"]);
    assert_eq!(
        rows(&mut e, "SELECT tmplname FROM pg_ts_template ORDER BY tmplname"),
        vec!["simple", "snowball"]
    );
    // The oids are PG's own for exactly these rows, so a tool that joins
    // on them lands where it expects.
    assert_eq!(
        rows(
            &mut e,
            "SELECT oid, cfgname FROM pg_ts_config ORDER BY oid"
        ),
        vec!["3748|simple", "13248|english"]
    );
    // …and every dictionary's template resolves.
    assert_eq!(
        one(
            &mut e,
            "SELECT count(*) FROM pg_ts_dict d LEFT JOIN pg_ts_template t \
             ON t.oid = d.dicttemplate WHERE t.oid IS NULL"
        ),
        "0"
    );
    // …as does every configuration's parser.
    assert_eq!(
        one(
            &mut e,
            "SELECT count(*) FROM pg_ts_config c LEFT JOIN pg_ts_parser p \
             ON p.oid = c.cfgparser WHERE p.oid IS NULL"
        ),
        "0"
    );
}

/// Registering a catalog took four separate lists; these are the two
/// that a `pg_class` row alone does not cover.
#[test]
fn round650_the_new_catalogs_are_reachable_every_way() {
    let mut e = Engine::new();
    assert_eq!(one(&mut e, "SELECT 'pg_ts_config'::regclass::oid"), "3602");
    assert_eq!(
        one(
            &mut e,
            "SELECT string_agg(attname, ',' ORDER BY attnum) FROM pg_attribute \
             WHERE attrelid = 'pg_ts_config'::regclass AND attnum > 0"
        ),
        "oid,cfgname,cfgnamespace,cfgowner,cfgparser"
    );
    assert_eq!(
        rows(
            &mut e,
            "SELECT relname FROM pg_class WHERE relname LIKE 'pg_ts%' ORDER BY relname"
        ),
        vec![
            "pg_ts_config",
            // v7.39 (round 651) — the token-type model arrived, so this
            // one is publishable and published.
            "pg_ts_config_map",
            "pg_ts_dict",
            "pg_ts_parser",
            "pg_ts_template"
        ]
    );
}

/// The pin that flipped. Round 650 recorded the token-type model as
/// absent and refused to stub these; round 651 built it, and both
/// answer from the same `TokenType` the tokenizer uses.
#[test]
fn round651_the_token_type_model_arrived() {
    let mut e = Engine::new();
    assert_eq!(
        one(&mut e, "SELECT count(*) FROM pg_ts_config_map"),
        "38",
        "nineteen mapped types per configuration, two configurations"
    );
    assert_eq!(
        one(&mut e, "SELECT count(*) FROM ts_token_type('default')"),
        "23"
    );
}
