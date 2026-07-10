//! v7.37.17 (17.6 siblings) — pgcrypto crypt() real md5crypt +
//! gen_salt('md5') real random salt.

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
fn crypt_md5_known_vector() {
    let mut e = Engine::new();
    // Vector verified against `openssl passwd -1 -salt saltsalt
    // password` on this machine.
    assert_eq!(
        text(&first(&mut e, "SELECT crypt('password', '$1$saltsalt$')")),
        "$1$saltsalt$qjXMvbEw8oaL.CzflDtaK/"
    );
}

#[test]
fn crypt_verify_roundtrip() {
    let mut e = Engine::new();
    // PG idiom: crypt(pw, stored_hash) == stored_hash verifies.
    let hashed = text(&first(&mut e, "SELECT crypt('s3cret', gen_salt('md5'))"));
    assert!(hashed.starts_with("$1$"), "hash shape: {hashed}");
    let verify_sql = format!("SELECT crypt('s3cret', '{hashed}')");
    assert_eq!(text(&first(&mut e, &verify_sql)), hashed);
    // Wrong password fails to match.
    let wrong_sql = format!("SELECT crypt('wrong', '{hashed}')");
    assert_ne!(text(&first(&mut e, &wrong_sql)), hashed);
}

#[test]
fn gen_salt_md5_shape_and_randomness() {
    let mut e = Engine::new();
    let a = text(&first(&mut e, "SELECT gen_salt('md5')"));
    let b = text(&first(&mut e, "SELECT gen_salt('md5')"));
    assert!(a.starts_with("$1$") && a.ends_with('$') && a.len() == 12);
    assert_ne!(a, b, "two salts must differ");
}

#[test]
fn unsupported_schemes_error() {
    let mut e = Engine::new();
    assert!(e.execute("SELECT gen_salt('bf')").is_err());
    assert!(e.execute("SELECT gen_salt('des')").is_err());
    assert!(e.execute("SELECT gen_salt('bogus')").is_err());
    // bcrypt-format salt errors in crypt too.
    assert!(
        e.execute("SELECT crypt('pw', '$2a$06$abcdefghijklmnopqrstuv')")
            .is_err()
    );
}

#[test]
fn crypt_null_passthrough() {
    let mut e = Engine::new();
    for f in &[
        "crypt(NULL::text, '$1$xx$')",
        "crypt('pw', NULL::text)",
        "gen_salt(NULL::text)",
    ] {
        let sql = format!("SELECT {f}");
        assert!(
            matches!(first(&mut e, &sql), spg_storage::Value::Null),
            "SELECT {f} should be NULL"
        );
    }
}
