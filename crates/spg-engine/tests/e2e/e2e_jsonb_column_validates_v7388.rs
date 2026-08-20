//! v7.38.8 — a json/jsonb column rejects text that is not JSON.
//!
//! It did not. `INSERT INTO t VALUES ('{bad')` into a jsonb column was
//! accepted and stored the raw text (PG18: `invalid input syntax for
//! type json`), and every later read of that row raised instead — in
//! one case on the checkpoint thread, where writes keep being
//! acknowledged while nothing reaches disk.
//!
//! This is also the guarantee the accessors now rest on: with the
//! column boundary enforced, a `Value::Json` is valid by construction
//! and `->>` does not parse the whole document per row to find out.

use spg_engine::Engine;

fn setup() -> Engine {
    let mut e = Engine::new();
    e.execute("CREATE TABLE j (id INT NOT NULL, b JSONB, p JSON)")
        .unwrap();
    e
}

#[test]
fn jsonb_column_refuses_text_that_is_not_json() {
    let mut e = setup();
    let err = e
        .execute("INSERT INTO j VALUES (1, '{bad', NULL)")
        .expect_err("a jsonb column must refuse text that is not JSON");
    let msg = alloc_msg(&err);
    assert!(
        msg.contains("invalid input syntax for type json"),
        "expected PG's own message, got: {msg}"
    );
}

#[test]
fn json_column_refuses_text_that_is_not_json() {
    let mut e = setup();
    let err = e
        .execute("INSERT INTO j VALUES (1, NULL, '[1,2')")
        .expect_err("a json column must refuse text that is not JSON");
    // The message matters as much as the refusal. Reported through the
    // generic coercion path this read `expected Json, actual Text`,
    // which describes a conversion that is ordinarily fine and says
    // nothing about the document being malformed. A reader would go
    // looking for a missing cast. PG names the real problem, and a
    // negative control that only asserts "it raised" cannot tell the
    // two apart — before this change the insert raised too, for the
    // wrong reason.
    let msg = alloc_msg(&err);
    assert!(
        msg.contains("invalid input syntax for type json"),
        "expected PG's own message, got: {msg}"
    );
}

#[test]
fn update_refuses_text_that_is_not_json() {
    let mut e = setup();
    e.execute("INSERT INTO j VALUES (1, '{\"a\":1}', '{\"a\":1}')")
        .unwrap();
    e.execute("UPDATE j SET b = '{bad' WHERE id = 1")
        .expect_err("an UPDATE into a jsonb column must refuse it too");
    // and the row is unchanged
    let r = e.execute("SELECT b->>'a' FROM j WHERE id = 1").unwrap();
    assert_eq!(format!("{:?}", r).contains('1'), true);
}

#[test]
fn valid_json_still_goes_in_and_reads_back() {
    let mut e = setup();
    e.execute("INSERT INTO j VALUES (1, '{\"a\": 1, \"b\": \"x\"}', '[1, 2]')")
        .unwrap();
    let r = e.execute("SELECT b->>'b' FROM j WHERE id = 1").unwrap();
    assert!(
        format!("{r:?}").contains('x'),
        "the accessor must still answer: {r:?}"
    );
}

#[test]
fn text_operand_is_still_validated_by_the_accessor() {
    // SPG accepts `text -> key` where PG has no such operator at all.
    // That operand has passed through no column boundary, so the
    // accessor is the only place it can be checked — and it still is.
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT NOT NULL, s TEXT)")
        .unwrap();
    e.execute("INSERT INTO t VALUES (1, '{bad')").unwrap();
    e.execute("SELECT s->>'a' FROM t WHERE id = 1")
        .expect_err("a text operand that is not JSON must still raise");
}

fn alloc_msg(e: &impl core::fmt::Debug) -> String {
    format!("{e:?}")
}

/// v7.38.8 — the key comparison in `locate_member` stopped decoding
/// every key it walks past. A key spelled with an escape must still
/// match the string it denotes, and the escape's own source text must
/// not be mistaken for the key.
#[test]
fn escaped_keys_still_match_what_they_denote() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE k (id INT NOT NULL, j JSON)")
        .unwrap();
    // The key token is `\u0061b`, which denotes `ab`.
    e.execute(r#"INSERT INTO k VALUES (1, '{"\u0061b": "found"}')"#)
        .unwrap();
    let hit = e.execute("SELECT j->>'ab' FROM k WHERE id = 1").unwrap();
    assert!(
        format!("{hit:?}").contains("found"),
        "an escaped key must match the string it denotes: {hit:?}"
    );
    // ... and its SOURCE TEXT is not the key. A byte comparison that
    // skipped the escape check would answer this one wrongly.
    let miss = e
        .execute(r#"SELECT j->>'\u0061b' FROM k WHERE id = 1"#)
        .unwrap();
    assert!(
        !format!("{miss:?}").contains("found"),
        "the escape's source text is not the key: {miss:?}"
    );
    // A quote inside a key, which the in-place comparison must not
    // mistake for the token's own closing quote.
    e.execute(r#"INSERT INTO k VALUES (2, '{"a\"b": "q"}')"#)
        .unwrap();
    let q = e
        .execute(r#"SELECT j->>'a"b' FROM k WHERE id = 2"#)
        .unwrap();
    assert!(
        format!("{q:?}").contains('q'),
        "a key containing a quote must still match: {q:?}"
    );
}

/// PG resolves a duplicate key to the LAST occurrence. The scan must
/// not stop at the first match — a faster loop that returned early
/// would answer differently and no other test here would notice.
#[test]
fn duplicate_keys_resolve_to_the_last() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE d (id INT NOT NULL, j JSON)")
        .unwrap();
    e.execute(r#"INSERT INTO d VALUES (1, '{"a": 1, "a": 2}')"#)
        .unwrap();
    let r = e.execute("SELECT j->>'a' FROM d WHERE id = 1").unwrap();
    assert!(
        format!("{r:?}").contains('2'),
        "the last occurrence wins: {r:?}"
    );
}
