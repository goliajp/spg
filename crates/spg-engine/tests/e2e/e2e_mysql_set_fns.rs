//! v7.37.17 (17.6 siblings) — MySQL quote / export_set / make_set +
//! truncate alias.

use spg_engine::{Engine, QueryResult};

fn first(e: &mut Engine, sql: &str) -> spg_storage::Value<'static> {
    let r = e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err:?}"));
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
fn quote_escapes() {
    let mut e = Engine::new();
    // MySQL doc vector: QUOTE('Don''t!') = 'Don\'t!'.
    assert_eq!(
        text(&first(&mut e, "SELECT quote('Don''t!')")),
        "'Don\\'t!'"
    );
    assert_eq!(text(&first(&mut e, "SELECT quote('plain')")), "'plain'");
    // SQL NULL renders as the word NULL.
    assert_eq!(text(&first(&mut e, "SELECT quote(NULL::text)")), "NULL");
}

#[test]
fn export_set_bits() {
    let mut e = Engine::new();
    // MySQL doc vector: EXPORT_SET(5, 'Y', 'N', ',', 4) = 'Y,N,Y,N'.
    assert_eq!(
        text(&first(
            &mut e,
            "SELECT export_set(5, 'Y', 'N', ',', 4)"
        )),
        "Y,N,Y,N"
    );
    // MySQL doc vector: EXPORT_SET(6, '1', '0', ',', 10).
    assert_eq!(
        text(&first(
            &mut e,
            "SELECT export_set(6, '1', '0', ',', 10)"
        )),
        "0,1,1,0,0,0,0,0,0,0"
    );
}

#[test]
fn make_set_members() {
    let mut e = Engine::new();
    // MySQL doc vector: MAKE_SET(1|4, 'hello', 'nice', 'world')
    // = 'hello,world'.
    assert_eq!(
        text(&first(
            &mut e,
            "SELECT make_set(5, 'hello', 'nice', 'world')"
        )),
        "hello,world"
    );
    assert_eq!(
        text(&first(&mut e, "SELECT make_set(1, 'a', 'b')")),
        "a"
    );
    assert_eq!(text(&first(&mut e, "SELECT make_set(0, 'a', 'b')")), "");
}

#[test]
fn truncate_alias() {
    let mut e = Engine::new();
    // MySQL doc vectors: TRUNCATE(1.223, 1) = 1.2,
    // TRUNCATE(122, -2) = 100. A decimal input yields DECIMAL (like MySQL and
    // PG's numeric trunc), not double — the exact 1.2 rides a Numeric now.
    match first(&mut e, "SELECT truncate(1.223, 1)") {
        spg_storage::Value::Numeric { scaled, scale , .. } => assert_eq!((scaled, scale), (12, 1)),
        other => panic!("got {other:?}"),
    }
    match first(&mut e, "SELECT truncate(122, -2)") {
        spg_storage::Value::Float(f) => assert!((f - 100.0).abs() < 1e-9),
        other => panic!("got {other:?}"),
    }
}

#[test]
fn set_fns_null_passthrough() {
    let mut e = Engine::new();
    for f in &[
        "export_set(NULL::int, 'Y', 'N')",
        "make_set(NULL::int, 'a')",
    ] {
        let sql = format!("SELECT {f}");
        assert!(
            matches!(first(&mut e, &sql), spg_storage::Value::Null),
            "SELECT {f} should be NULL"
        );
    }
}
