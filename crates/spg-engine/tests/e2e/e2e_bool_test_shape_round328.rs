//! read01 round 328 (V45) — `x IS [NOT] TRUE | FALSE | UNKNOWN` survives
//! as what the user wrote.
//!
//! The parser lowered these into `CASE WHEN … THEN TRUE ELSE FALSE END`
//! (and `IS UNKNOWN` into `IS NULL`) before the AST ever saw them. The
//! semantics were right; the form was gone, so every renderer printed the
//! lowering — `CHECK ((a > 1) IS TRUE)` came back as `CHECK ((CASE WHEN
//! (a > 1) THEN TRUE ELSE FALSE END))`, which is what a dump carried.
//!
//! Measured on PG 18.4:
//!   * `pg_get_constraintdef` → `CHECK (((a > 1) IS TRUE))`,
//!     `CHECK ((b IS NOT FALSE))`, `CHECK (((a > 1) IS NOT TRUE))`,
//!     `CHECK ((b IS UNKNOWN))`
//!   * `NULL::bool IS TRUE` false, `IS NOT TRUE` true, `IS UNKNOWN` true,
//!     `false IS NOT FALSE` false — none of them ever answers NULL.

use spg_engine::Engine;
use spg_storage::Value;

fn row(e: &mut Engine, sql: &str) -> Vec<Value<'static>> {
    match e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err}")) {
        spg_engine::QueryResult::Rows { rows, .. } => rows
            .first()
            .map(|r| r.values.iter().cloned().map(Value::into_owned).collect())
            .unwrap_or_else(|| panic!("no row for `{sql}`")),
        other => panic!("`{sql}` did not return rows: {other:?}"),
    }
}

fn constraintdef(e: &mut Engine, name: &str) -> String {
    let sql =
        format!("SELECT pg_get_constraintdef(oid) FROM pg_constraint WHERE conname = '{name}'");
    match row(e, &sql).into_iter().next() {
        Some(Value::Text(t)) => t.to_string(),
        other => panic!("no constraintdef for {name}: {other:?}"),
    }
}

fn fixture() -> Engine {
    let mut e = Engine::new();
    e.execute(
        "CREATE TABLE v45 (a INT, b BOOL, \
         CONSTRAINT c1 CHECK ((a > 1) IS TRUE), \
         CONSTRAINT c2 CHECK (b IS NOT FALSE), \
         CONSTRAINT c3 CHECK ((a > 1) IS NOT TRUE), \
         CONSTRAINT c4 CHECK (b IS UNKNOWN))",
    )
    .unwrap();
    e
}

/// The reflection surface a dump reads.
#[test]
fn a_check_constraint_echoes_the_boolean_test_it_was_given() {
    let mut e = fixture();
    assert_eq!(constraintdef(&mut e, "c1"), "CHECK (((a > 1) IS TRUE))");
    assert_eq!(constraintdef(&mut e, "c2"), "CHECK ((b IS NOT FALSE))");
    assert_eq!(constraintdef(&mut e, "c3"), "CHECK (((a > 1) IS NOT TRUE))");
    assert_eq!(constraintdef(&mut e, "c4"), "CHECK ((b IS UNKNOWN))");
}

/// The three-valued semantics are unchanged — none of these answers NULL.
#[test]
fn the_three_valued_semantics_match_pg() {
    let mut e = Engine::new();
    assert_eq!(
        row(
            &mut e,
            "SELECT NULL::bool IS TRUE, NULL::bool IS NOT TRUE, \
             NULL::bool IS UNKNOWN, false IS NOT FALSE"
        ),
        vec![
            Value::Bool(false),
            Value::Bool(true),
            Value::Bool(true),
            Value::Bool(false)
        ],
    );
    assert_eq!(
        row(
            &mut e,
            "SELECT true IS TRUE, true IS FALSE, false IS FALSE, \
             NULL::bool IS NOT UNKNOWN"
        ),
        vec![
            Value::Bool(true),
            Value::Bool(false),
            Value::Bool(true),
            Value::Bool(false)
        ],
    );
}

/// And they still filter rows the same way.
#[test]
fn a_boolean_test_still_filters() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT NOT NULL, b BOOL)")
        .unwrap();
    e.execute("INSERT INTO t VALUES (1, true), (2, false), (3, NULL)")
        .unwrap();
    assert_eq!(
        row(&mut e, "SELECT count(*) FROM t WHERE b IS TRUE"),
        vec![Value::BigInt(1)]
    );
    assert_eq!(
        row(&mut e, "SELECT count(*) FROM t WHERE b IS NOT TRUE"),
        vec![Value::BigInt(2)],
        "false and NULL are both `not true`"
    );
    assert_eq!(
        row(&mut e, "SELECT count(*) FROM t WHERE b IS UNKNOWN"),
        vec![Value::BigInt(1)]
    );
}

/// The CHECK still enforces what it says.
#[test]
fn the_check_constraint_still_enforces() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE g (a INT, CONSTRAINT ck CHECK ((a > 1) IS TRUE))")
        .unwrap();
    e.execute("INSERT INTO g VALUES (2)").unwrap();
    assert!(
        e.execute("INSERT INTO g VALUES (0)").is_err(),
        "0 > 1 is false, so the test fails"
    );
    assert!(
        e.execute("INSERT INTO g VALUES (NULL)").is_err(),
        "NULL > 1 is unknown, which IS TRUE rejects"
    );
}
