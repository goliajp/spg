//! v7.38 (read01 P5.24) — gen_random_bytes / gen_salt draw from the host
//! CSPRNG (the server injects /dev/urandom) rather than the predictable
//! process-static xorshift PRNG. We prove the routing by injecting a known
//! salt function and observing its bytes in the output.

use spg_engine::{Engine, QueryResult};

fn known_salt() -> [u8; 16] {
    [0xAB; 16]
}

fn text(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => match &rows[0].values[0] {
            spg_storage::Value::Text(s) => s.to_string(),
            v => panic!("expected text, got {v:?}"),
        },
        _ => panic!("expected rows"),
    }
}

#[test]
fn gen_random_bytes_uses_injected_csprng() {
    let mut e = Engine::new().with_salt_fn(known_salt);
    // With the CSPRNG returning 0xAB repeatedly, the bytes are 0xABABABAB.
    assert_eq!(
        text(&mut e, "SELECT encode(gen_random_bytes(4), 'hex')"),
        "abababab"
    );
    // Length still honoured across multiple 16-byte draws.
    let len = match e
        .execute("SELECT length(gen_random_bytes(40))::int")
        .unwrap()
    {
        QueryResult::Rows { rows, .. } => rows[0].values[0].clone(),
        _ => panic!(),
    };
    assert_eq!(len, spg_storage::Value::Int(40));
    // gen_salt routes through the same CSPRNG (deterministic from 0xAB here).
    let salt = text(&mut e, "SELECT gen_salt('md5')");
    assert!(salt.starts_with("$1$") && salt.ends_with('$'));

    // Without a host CSPRNG the PRNG fallback still produces output.
    let mut e0 = Engine::new();
    let n = match e0
        .execute("SELECT length(gen_random_bytes(8))::int")
        .unwrap()
    {
        QueryResult::Rows { rows, .. } => rows[0].values[0].clone(),
        _ => panic!(),
    };
    assert_eq!(n, spg_storage::Value::Int(8));
}
