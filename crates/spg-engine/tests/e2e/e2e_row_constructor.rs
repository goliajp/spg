//! Bare ROW(a, b, …) constructor — PG record-text rendering, plus
//! the ROW keyword joining the paren row-comparison machinery.

use spg_engine::{Engine, QueryResult};

fn one(e: &mut Engine, sql: &str) -> spg_storage::Value<'static> {
    let r = e
        .execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    let QueryResult::Rows { rows, .. } = r else {
        panic!("expected Rows");
    };
    rows[0].values[0].clone()
}

fn text(e: &mut Engine, sql: &str) -> String {
    match one(e, sql) {
        spg_storage::Value::Text(s) => s.to_string(),
        v => panic!("{sql}: expected Text, got {v:?}"),
    }
}

#[test]
fn bare_paren_row_constructor_is_a_value() {
    // read01 (composite) — a bare `(a, b, …)` not followed by a comparison /
    // [NOT] IN / IS [NOT] NULL is a row constructor value, identical to the
    // `ROW(a, b, …)` keyword form: it renders as PG record text and takes
    // postfix `::text` / `.field`. Values live-PG18.4.
    let mut e = Engine::new();
    assert_eq!(text(&mut e, "SELECT (1,'a')::text"), "(1,a)");
    assert_eq!(text(&mut e, "SELECT (1,2,3)::text"), "(1,2,3)");
    // Bare (no cast) still evaluates to a composite that renders as record text
    // through a whole-row JSON round-trip.
    assert_eq!(
        text(&mut e, "SELECT row_to_json((1,'a'))::text"),
        "{\"f1\":1,\"f2\":\"a\"}"
    );
    // Field access on the parenthesised composite (SPG's `(expr).field`).
    assert_eq!(one(&mut e, "SELECT (1,'a').f1"), spg_storage::Value::Int(1));

    // Regression: the row-comparison / predicate forms are untouched — they
    // still return before the constructor path.
    assert!(survives(&mut e, "(1,2) = (1,2)"));
    assert!(survives(&mut e, "(1,2) IN ((1,2),(3,4))"));
    assert!(survives(&mut e, "(1,2) < (1,3)"));
    assert!(!survives(&mut e, "(1,2) IS NULL"));
    // A single-element paren stays a plain expression, not a 1-tuple.
    assert_eq!(one(&mut e, "SELECT (5)"), spg_storage::Value::Int(5));
    assert_eq!(one(&mut e, "SELECT (1+2)*3"), spg_storage::Value::Int(9));
}

fn survives(e: &mut Engine, cond: &str) -> bool {
    let sql = alloc_sql(cond);
    let r = e
        .execute(&sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    let QueryResult::Rows { rows, .. } = r else {
        panic!("expected Rows");
    };
    !rows.is_empty()
}

fn alloc_sql(cond: &str) -> String {
    format!("SELECT 1 WHERE {cond}")
}

#[test]
fn bare_row_renders_record_text() {
    // v7.38 (read01, T9) — ROW(...) is a first-class composite value; its text
    // form is PG's record_out. Assert the rendered text via ::text.
    let mut e = Engine::new();
    let spg_storage::Value::Text(s) = one(&mut e, "SELECT (ROW(1, 'a'))::text") else {
        panic!("expected Text");
    };
    assert_eq!(s.as_ref(), "(1,a)");
    // NULL field renders empty; special characters get quoted.
    let spg_storage::Value::Text(s) = one(&mut e, "SELECT (ROW(1, NULL, 'x,y'))::text") else {
        panic!("expected Text");
    };
    assert_eq!(s.as_ref(), "(1,,\"x,y\")");
    // Single-element ROW is valid (unlike bare parens).
    let spg_storage::Value::Text(s) = one(&mut e, "SELECT (ROW(7))::text") else {
        panic!("expected Text");
    };
    assert_eq!(s.as_ref(), "(7)");
    // The bare value is a composite record.
    assert!(matches!(
        one(&mut e, "SELECT ROW(1, 'a')"),
        spg_storage::Value::Composite(_)
    ));
}

#[test]
fn row_keyword_comparisons() {
    let mut e = Engine::new();
    assert!(survives(&mut e, "ROW(1, 2) = ROW(1, 2)"));
    assert!(!survives(&mut e, "ROW(1, 2) = ROW(1, 3)"));
    assert!(survives(&mut e, "ROW(1, 2) < ROW(1, 3)"));
    assert!(survives(&mut e, "ROW(1, 2) < (2, 0)"));
    assert!(survives(&mut e, "ROW(1, 2) IN ((3, 4), ROW(1, 2))"));
    assert!(!survives(&mut e, "ROW(1, 2) NOT IN ((1, 2))"));
    // Arity mismatch errors.
    assert!(e.execute("SELECT 1 WHERE ROW(1, 2) = ROW(1)").is_err());
}
