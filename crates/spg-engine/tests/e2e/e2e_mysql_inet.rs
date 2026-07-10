//! v7.37.17 (17.6 siblings) — MySQL network address functions +
//! INSERT() string function.

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

fn text(v: &spg_storage::Value<'_>) -> String {
    match v {
        spg_storage::Value::Text(s) => s.to_string(),
        other => panic!("expected Text, got {other:?}"),
    }
}

#[test]
fn inet_aton_ntoa_roundtrip() {
    let mut e = Engine::new();
    // MySQL doc vector: INET_ATON('10.0.5.9') → 167773449.
    assert!(matches!(
        first(&mut e, "SELECT inet_aton('10.0.5.9')"),
        spg_storage::Value::BigInt(167773449)
    ));
    // MySQL doc vector: INET_NTOA(167773449) → '10.0.5.9'.
    assert_eq!(
        text(&first(&mut e, "SELECT inet_ntoa(167773449)")),
        "10.0.5.9"
    );
    // Invalid input → NULL, not an error.
    assert!(matches!(
        first(&mut e, "SELECT inet_aton('not an ip')"),
        spg_storage::Value::Null
    ));
}

#[test]
fn inet6_aton_ntoa_roundtrip() {
    let mut e = Engine::new();
    // IPv6 round-trips through the 16-byte binary form with RFC
    // 5952 compression on the way out.
    assert_eq!(
        text(&first(
            &mut e,
            "SELECT inet6_ntoa(inet6_aton('fdfe:0:0:0:5a55:caff:fefa:9089'))"
        )),
        "fdfe::5a55:caff:fefa:9089"
    );
    // IPv4 input keeps the 4-byte form (MySQL semantics).
    assert_eq!(
        text(&first(&mut e, "SELECT inet6_ntoa(inet6_aton('10.0.5.9'))")),
        "10.0.5.9"
    );
}

#[test]
fn is_ipv4_is_ipv6() {
    let mut e = Engine::new();
    assert!(matches!(
        first(&mut e, "SELECT is_ipv4('10.0.5.9')"),
        spg_storage::Value::Bool(true)
    ));
    assert!(matches!(
        first(&mut e, "SELECT is_ipv4('10.0.5.256')"),
        spg_storage::Value::Bool(false)
    ));
    assert!(matches!(
        first(&mut e, "SELECT is_ipv6('::1')"),
        spg_storage::Value::Bool(true)
    ));
    assert!(matches!(
        first(&mut e, "SELECT is_ipv6('10.0.5.9')"),
        spg_storage::Value::Bool(false)
    ));
}

#[test]
fn mysql_insert_function() {
    let mut e = Engine::new();
    // MySQL doc vector: INSERT('Quadratic', 3, 4, 'What') → 'QuWhattic'.
    assert_eq!(
        text(&first(&mut e, "SELECT insert('Quadratic', 3, 4, 'What')")),
        "QuWhattic"
    );
    // MySQL doc vector: INSERT('Quadratic', -1, 4, 'What') → 'Quadratic'.
    assert_eq!(
        text(&first(&mut e, "SELECT insert('Quadratic', -1, 4, 'What')")),
        "Quadratic"
    );
    // MySQL doc vector: INSERT('Quadratic', 3, 100, 'What') → 'QuWhat'.
    assert_eq!(
        text(&first(&mut e, "SELECT insert('Quadratic', 3, 100, 'What')")),
        "QuWhat"
    );
}
