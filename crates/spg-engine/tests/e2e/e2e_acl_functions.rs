//! v7.37.17 (17.6 siblings) — acldefault + makeaclitem + object
//! addressing probes.

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

fn text_array(v: &spg_storage::Value<'_>) -> Vec<Option<String>> {
    match v {
        spg_storage::Value::TextArray(items) => items.clone(),
        other => panic!("expected TextArray, got {other:?}"),
    }
}

#[test]
fn acldefault_per_object_type() {
    let mut e = Engine::new();
    // v7.39 (round 522) — these three asserted SPG's own output: no
    // MAINTAIN privilege, no PUBLIC entry, and `admin` as the owner of
    // whatever oid was asked about. Every value here is a PG18 reading
    // with owner oid 10, which SPG publishes as `postgres`.
    assert_eq!(
        text_array(&first(&mut e, "SELECT acldefault('r', 10)")),
        vec![Some("postgres=arwdDxtm/postgres".to_string())]
    );
    assert_eq!(
        text_array(&first(&mut e, "SELECT acldefault('f', 10)")),
        vec![
            Some("=X/postgres".to_string()),
            Some("postgres=X/postgres".to_string())
        ]
    );
    assert_eq!(
        text_array(&first(&mut e, "SELECT acldefault('n', 10)")),
        vec![Some("postgres=UC/postgres".to_string())]
    );
    // Unknown object type errors.
    assert!(e.execute("SELECT acldefault('z', 10)").is_err());
}

#[test]
fn makeaclitem_builds_text_form() {
    let mut e = Engine::new();
    assert_eq!(
        text(&first(
            &mut e,
            "SELECT makeaclitem(10, 10, 'SELECT', false)"
        )),
        "admin=r/admin"
    );
    // Multiple privileges, comma-separated.
    assert_eq!(
        text(&first(
            &mut e,
            "SELECT makeaclitem(10, 10, 'SELECT, UPDATE', false)"
        )),
        "admin=rw/admin"
    );
    // Grantable appends '*' per privilege.
    assert_eq!(
        text(&first(&mut e, "SELECT makeaclitem(10, 10, 'INSERT', true)")),
        "admin=a*/admin"
    );
    // Grantee 0 = PUBLIC (empty name).
    assert_eq!(
        text(&first(&mut e, "SELECT makeaclitem(0, 10, 'SELECT', false)")),
        "=r/admin"
    );
    // Unknown privilege errors.
    assert!(
        e.execute("SELECT makeaclitem(10, 10, 'FLY', false)")
            .is_err()
    );
}

#[test]
fn object_addressing_probes_return_null() {
    let mut e = Engine::new();
    for f in &[
        "pg_describe_object(1259, 1, 0)",
        "pg_identify_object(1259, 1, 0)",
        "pg_identify_object_as_address(1259, 1, 0)",
        "pg_get_object_address('table', ARRAY['t'], ARRAY[]::text[])",
    ] {
        let sql = format!("SELECT {f}");
        assert!(
            matches!(first(&mut e, &sql), spg_storage::Value::Null),
            "SELECT {f} should be NULL"
        );
    }
}

#[test]
fn acl_null_passthrough() {
    let mut e = Engine::new();
    for f in &[
        "acldefault(NULL::text, 10)",
        "makeaclitem(10, 10, NULL::text, false)",
    ] {
        let sql = format!("SELECT {f}");
        assert!(
            matches!(first(&mut e, &sql), spg_storage::Value::Null),
            "SELECT {f} should be NULL"
        );
    }
}
