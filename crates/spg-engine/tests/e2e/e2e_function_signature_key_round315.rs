//! v7.39 (round 315, V19) — a signature keys the same however it is
//! written.
//!
//! `function_arg_types` split every argument on "two or more words means
//! `name TYPE`", so a bare multi-word type lost its first word:
//! `f(double precision)` keyed as `f(precision)` while
//! `f(x double precision)` keyed as `f(float)`. The same signature
//! written two ways did not resolve to the same function — PG treats
//! them as one (measured: the second CREATE OR REPLACE replaces the
//! first, leaving a single overload).
//!
//! Changing the formula is the reason this waited for its own round. The
//! function catalogue recomputes its keys on load, so it migrates
//! itself; the ACL block does NOT — it persists the computed key and
//! matches on it. An older image's key would simply fail to match, and
//! the owner and grants would be dropped without a word. The loader
//! therefore falls back to the pre-fix formula, and that fallback is
//! pinned here directly.

use spg_engine::{Engine, QueryResult};
use spg_storage::{
    FunctionDef, function_signature_key, function_signature_key_legacy, resolve_stored_function_key,
};

/// Every multi-word bare type PG accepts, each paired with its named
/// spelling. The two must key identically.
#[test]
fn a_bare_multiword_type_keys_like_its_named_spelling() {
    for (bare, named) in [
        ("(double precision)", "(x double precision)"),
        ("(character varying)", "(s character varying)"),
        ("(bit varying)", "(b bit varying)"),
        (
            "(timestamp with time zone)",
            "(ts timestamp with time zone)",
        ),
        (
            "(timestamp without time zone)",
            "(ts timestamp without time zone)",
        ),
        ("(time with time zone)", "(t time with time zone)"),
        ("(time without time zone)", "(t time without time zone)"),
        ("(national character)", "(n national character)"),
        (
            "(national character varying)",
            "(n national character varying)",
        ),
        // A modifier must not change the answer either.
        ("(bit varying(8))", "(b bit varying(8))"),
        // Several arguments, one of them multi-word.
        ("(int, double precision)", "(a int, b double precision)"),
        // A mode prefix is still not part of the type.
        ("(OUT double precision)", "(OUT x double precision)"),
    ] {
        assert_eq!(
            function_signature_key("f", bare),
            function_signature_key("f", named),
            "{bare} vs {named}"
        );
    }
}

/// The single-word cases were never broken and must stay put.
#[test]
fn ordinary_types_are_unaffected() {
    assert_eq!(function_signature_key("f", "(int)"), "f(int)");
    assert_eq!(function_signature_key("f", "(x int)"), "f(int)");
    assert_eq!(function_signature_key("f", "()"), "f()");
    assert_eq!(
        function_signature_key("f", "(int, text)"),
        function_signature_key("f", "(a int, b text)")
    );
    // Two genuinely different signatures must NOT collide.
    assert_ne!(
        function_signature_key("f", "(int)"),
        function_signature_key("f", "(text)")
    );
    assert_ne!(
        function_signature_key("f", "(double precision)"),
        function_signature_key("f", "(real)")
    );
}

/// The legacy formula has to keep reproducing the OLD key exactly, or
/// the migration has nothing to recognise. These are the keys images
/// written before this round contain.
#[test]
fn the_legacy_formula_reproduces_the_old_keys() {
    assert_eq!(
        function_signature_key_legacy("f", "(double precision)"),
        "f(precision)"
    );
    assert_eq!(
        function_signature_key_legacy("g", "(character varying)"),
        "g(varying)"
    );
    assert_eq!(
        function_signature_key_legacy("h", "(timestamp with time zone)"),
        "h(with time zone)"
    );
    // Where the old formula was already right, both agree — those images
    // match exactly and never reach the fallback.
    assert_eq!(
        function_signature_key_legacy("f", "(x double precision)"),
        function_signature_key("f", "(x double precision)")
    );
    assert_eq!(
        function_signature_key_legacy("i", "(int)"),
        function_signature_key("i", "(int)")
    );
}

/// The migration itself: a key an older image wrote still finds its
/// function, so its owner and grants survive the upgrade.
#[test]
fn an_old_images_acl_key_still_resolves() {
    let mut functions = alloc_map();
    let def = FunctionDef {
        name: "f".into(),
        args_repr: "(double precision)".into(),
        returns: "int".into(),
        language: "sql".into(),
        body: " SELECT 1 ".into(),
        owner: None,
        acl: Vec::new(),
        volatility: spg_storage::FN_VOLATILE,
        strict: false,
        security_definer: false,
        leakproof: false,
        parallel: spg_storage::FN_PARALLEL_UNSAFE,
        cost: None,
        rows: None,
    };
    let new_key = function_signature_key(&def.name, &def.args_repr);
    functions.insert(new_key.clone(), def);

    // What an image written before this round holds.
    let old_key = "f(precision)";
    assert_ne!(old_key, new_key, "the fixture must exercise the migration");
    assert_eq!(
        resolve_stored_function_key(&functions, old_key),
        Some(new_key.clone()),
        "an old key must still find its function"
    );
    // A current image matches outright.
    assert_eq!(
        resolve_stored_function_key(&functions, &new_key),
        Some(new_key)
    );
    // A key for something that genuinely is not there stays unmatched —
    // the fallback must not start attaching grants to the wrong function.
    assert_eq!(resolve_stored_function_key(&functions, "nosuch(int)"), None);
}

fn alloc_map() -> std::collections::BTreeMap<String, FunctionDef> {
    std::collections::BTreeMap::new()
}

/// The parser had the same bug from the other side: it read at most two
/// words per argument, so it could not spell `x double precision` at
/// all and silently mis-read the bare `double precision` as a parameter
/// named "double". Both halves had to move or the fix is unreachable
/// from SQL.
#[test]
fn every_multiword_spelling_parses() {
    for sql in [
        "CREATE FUNCTION f(double precision) RETURNS int LANGUAGE sql AS $$ SELECT 1 $$",
        "CREATE FUNCTION f(x double precision) RETURNS int LANGUAGE sql AS $$ SELECT 1 $$",
        "CREATE FUNCTION f(character varying) RETURNS int LANGUAGE sql AS $$ SELECT 1 $$",
        "CREATE FUNCTION f(s character varying) RETURNS int LANGUAGE sql AS $$ SELECT 1 $$",
        "CREATE FUNCTION f(timestamp with time zone) RETURNS int LANGUAGE sql AS $$ SELECT 1 $$",
        "CREATE FUNCTION f(ts timestamp with time zone) RETURNS int LANGUAGE sql AS $$ SELECT 1 $$",
        "CREATE FUNCTION f(time without time zone) RETURNS int LANGUAGE sql AS $$ SELECT 1 $$",
        // The single-word forms must keep working.
        "CREATE FUNCTION f(int) RETURNS int LANGUAGE sql AS $$ SELECT 1 $$",
        "CREATE FUNCTION f(x int) RETURNS int LANGUAGE sql AS $$ SELECT 1 $$",
        "CREATE FUNCTION f() RETURNS int LANGUAGE sql AS $$ SELECT 1 $$",
        "CREATE FUNCTION f(a int, b double precision) RETURNS int LANGUAGE sql AS $$ SELECT 1 $$",
    ] {
        assert!(
            spg_sql::parser::parse_statement(sql).is_ok(),
            "{sql} must parse"
        );
    }
}

/// End to end: a function declared with a bare multi-word type is the
/// SAME function when referenced with a named one — which is what the
/// key is for.
#[test]
fn the_two_spellings_are_one_function_end_to_end() {
    let mut e = Engine::new();
    e.execute("CREATE FUNCTION f19(double precision) RETURNS int LANGUAGE sql AS $$ SELECT 1 $$")
        .unwrap();
    // PG replaces rather than overloading here; SPG must agree, which it
    // can only do if both spellings key the same.
    e.execute(
        "CREATE OR REPLACE FUNCTION f19(x double precision) RETURNS int LANGUAGE sql AS $$ SELECT 2 $$",
    )
    .unwrap();
    match e
        .execute("SELECT count(*) FROM pg_proc WHERE proname = 'f19'")
        .unwrap()
    {
        QueryResult::Rows { rows, .. } => assert_eq!(
            spg_engine::eval::value_to_text(&rows[0].values[0]),
            "1",
            "the two spellings must be one function, not two overloads"
        ),
        other => panic!("{other:?}"),
    }
}
