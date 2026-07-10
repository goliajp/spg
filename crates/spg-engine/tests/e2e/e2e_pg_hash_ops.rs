//! v7.37.17 (17.6 siblings) — pg_hash_* operator-support helpers.

use spg_engine::{Engine, QueryResult};

fn first(e: &mut Engine, sql: &str) -> spg_storage::Value<'static> {
    let r = e
        .execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    let QueryResult::Rows { rows, .. } = r else {
        panic!("expected Rows");
    };
    rows[0].values[0].clone()
}

fn as_int(v: &spg_storage::Value<'_>) -> i32 {
    match v {
        spg_storage::Value::Int(n) => *n,
        other => panic!("expected Int, got {other:?}"),
    }
}

#[test]
fn hashtext_is_deterministic() {
    let mut e = Engine::new();
    let a = as_int(&first(&mut e, "SELECT hashtext('hello')"));
    let b = as_int(&first(&mut e, "SELECT hashtext('hello')"));
    assert_eq!(a, b);
    // Different inputs → different hashes (unless collision, rare).
    let c = as_int(&first(&mut e, "SELECT hashtext('world')"));
    assert_ne!(a, c);
}

#[test]
fn hashint4_and_hashint8() {
    let mut e = Engine::new();
    let a = as_int(&first(&mut e, "SELECT hashint4(42)"));
    let b = as_int(&first(&mut e, "SELECT hashint4(42)"));
    assert_eq!(a, b);
    let c = as_int(&first(&mut e, "SELECT hashint4(0)"));
    assert_ne!(a, c);
    // int8 hash also stable.
    let d = as_int(&first(&mut e, "SELECT hashint8(42::bigint)"));
    let d2 = as_int(&first(&mut e, "SELECT hashint8(42::bigint)"));
    assert_eq!(d, d2);
}

#[test]
fn hashbytea_is_stable() {
    let mut e = Engine::new();
    let a = as_int(&first(&mut e, "SELECT hashbytea('hello'::bytea)"));
    let b = as_int(&first(&mut e, "SELECT hashbytea('hello'::bytea)"));
    assert_eq!(a, b);
}

#[test]
fn hash_null_passthrough() {
    let mut e = Engine::new();
    for f in &[
        "hashtext(NULL::text)",
        "hashint4(NULL::int)",
        "hashint8(NULL::bigint)",
        "hashbytea(NULL::bytea)",
    ] {
        let sql = format!("SELECT {f}");
        assert!(
            matches!(first(&mut e, &sql), spg_storage::Value::Null),
            "SELECT {f} should be NULL"
        );
    }
}
