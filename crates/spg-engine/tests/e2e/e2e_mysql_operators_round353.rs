//! read01 round 353 (MySQL differential, M9 + M10) — the operator tokens.
//!
//! `DIV` and `!` did not parse. Measuring them turned up three tokens
//! that mean DIFFERENT THINGS in the two dialects, all of which SPG read
//! PG's way on a MySQL session:
//!
//! | token | PG (and SPG before) | MariaDB 11, measured |
//! |---|---|---|
//! | `\|\|` | string concatenation | **OR** — `1 \|\| 0` is 1, not '10' |
//! | `&&` | inet / array overlap | **AND** |
//! | `<=>` | pgvector cosine distance | **NULL-safe equal** |
//!
//! `1 || 0` answering the string `'10'` is a wrong answer with no error,
//! which makes it the worst of the three.
//!
//! `!` and `NOT` also differ in PRECEDENCE, which is easy to miss:
//! MariaDB answers 1 for `!1 + 1` — that is `(!1)+1` — and 0 for
//! `NOT 1 + 1`, which is `NOT (1+1)`.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn mysql() -> Engine {
    let mut e = Engine::new();
    e.execute("SET sql_mode='STRICT_TRANS_TABLES'").unwrap();
    e
}

fn one(e: &mut Engine, sql: &str) -> Value<'static> {
    match e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err}")) {
        QueryResult::Rows { rows, .. } => rows
            .first()
            .and_then(|r| r.values.first())
            .cloned()
            .map(Value::into_owned)
            .unwrap_or(Value::Null),
        other => panic!("`{sql}` did not return rows: {other:?}"),
    }
}

/// `DIV` truncates toward zero and answers NULL on a zero divisor.
#[test]
fn div_truncates_toward_zero() {
    let mut e = mysql();
    for (sql, want) in [
        ("SELECT 5 DIV 2", 2),
        ("SELECT -7 DIV 2", -3),
        ("SELECT 7 DIV -2", -3),
        ("SELECT 7.5 DIV 2", 3),
        ("SELECT '9' DIV 2", 4),
    ] {
        assert_eq!(one(&mut e, sql), Value::BigInt(want), "for `{sql}`");
    }
    assert_eq!(one(&mut e, "SELECT 5 DIV 0"), Value::Null);
    // …and `/` is still the real division round 351 measured.
    assert_eq!(one(&mut e, "SELECT 5/2"), Value::Float(2.5));
}

/// `!` negates a truth value, and binds tighter than arithmetic.
#[test]
fn bang_negates_and_binds_tight() {
    let mut e = mysql();
    assert_eq!(one(&mut e, "SELECT !1"), Value::Bool(false));
    assert_eq!(one(&mut e, "SELECT !0"), Value::Bool(true));
    assert_eq!(one(&mut e, "SELECT !5"), Value::Bool(false));
    assert_eq!(one(&mut e, "SELECT !'abc'"), Value::Bool(true));
    assert_eq!(one(&mut e, "SELECT !NULL"), Value::Null);
    // The precedence difference, measured: `(!1)+1` vs `NOT (1+1)`.
    assert_eq!(one(&mut e, "SELECT !1 + 1"), Value::BigInt(1));
    assert_eq!(one(&mut e, "SELECT NOT 1 + 1"), Value::Bool(false));
}

/// The three tokens that change meaning with the dialect.
#[test]
fn the_conflicting_tokens_read_mysqls_way() {
    let mut e = mysql();
    // `||` is OR, not concatenation. This was answering '10'.
    assert_eq!(one(&mut e, "SELECT 1 || 0"), Value::Bool(true));
    assert_eq!(one(&mut e, "SELECT 0 || 0"), Value::Bool(false));
    // `&&` is AND, not inet overlap.
    assert_eq!(one(&mut e, "SELECT 1 && 1"), Value::Bool(true));
    assert_eq!(one(&mut e, "SELECT 1 && 0"), Value::Bool(false));
    // `<=>` is NULL-safe equality, not vector distance.
    assert_eq!(one(&mut e, "SELECT 1 <=> 1"), Value::Bool(true));
    assert_eq!(one(&mut e, "SELECT NULL <=> NULL"), Value::Bool(true));
    assert_eq!(one(&mut e, "SELECT 1 <=> NULL"), Value::Bool(false));
}

/// …and keep meaning PG's thing on a PG session.
#[test]
fn pg_keeps_its_own_readings() {
    let mut p = Engine::new();
    assert_eq!(one(&mut p, "SELECT 'a' || 'b'"), Value::text("ab"));
    assert!(
        p.execute("SELECT 5 DIV 2").is_err(),
        "DIV is not a PG operator"
    );
    assert!(p.execute("SELECT !1").is_err(), "nor is prefix !");
}
