//! v7.37.17 (17.6 siblings) — inet_merge + macaddr8_set7bit +
//! connection probes + timeofday.

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
    // v7.39 (read01 inet family) — inet_merge now returns a typed CIDR
    // (matching PG's return type), so render through the canonical form.
    spg_engine::eval::value_to_text(v)
}

#[test]
fn inet_merge_common_prefix() {
    let mut e = Engine::new();
    // PG doc example: inet_merge('192.168.1.5/24','192.168.2.5/24')
    // = 192.168.0.0/22.
    assert_eq!(
        text(&first(
            &mut e,
            "SELECT inet_merge('192.168.1.5/24', '192.168.2.5/24')"
        )),
        "192.168.0.0/22"
    );
    // Identical networks — capped by the input masks.
    assert_eq!(
        text(&first(
            &mut e,
            "SELECT inet_merge('10.0.0.1/8', '10.0.0.2/8')"
        )),
        "10.0.0.0/8"
    );
}

#[test]
fn inet_merge_mixed_family_errors() {
    let mut e = Engine::new();
    assert!(
        e.execute("SELECT inet_merge('192.168.1.5/24', '::1')")
            .is_err()
    );
}

#[test]
fn macaddr8_set7bit_sets_bit() {
    // v7.38 (read01) — PG's macaddr8_set7bit takes and returns a macaddr8; it
    // used to insist on TEXT and hand a TEXT back. An unadorned literal is the
    // unknown-type form PG casts for you. Oracle: live PG18.4.
    let mut e = Engine::new();
    // PG doc example: 00:34:56:ab:cd:ef:12:34 → 02:34:56:ab:cd:ef:12:34
    assert_eq!(
        text(&first(
            &mut e,
            "SELECT (macaddr8_set7bit('00:34:56:ab:cd:ef:12:34'))::text"
        )),
        "02:34:56:ab:cd:ef:12:34"
    );
    // A real macaddr8 argument works and keeps the macaddr8 type.
    assert_eq!(
        text(&first(
            &mut e,
            "SELECT (macaddr8_set7bit('00:34:56:ab:cd:ef:12:34'::macaddr8))::text"
        )),
        "02:34:56:ab:cd:ef:12:34"
    );
    assert_eq!(
        text(&first(
            &mut e,
            "SELECT pg_typeof(macaddr8_set7bit('00:34:56:ab:cd:ef:12:34'::macaddr8))::text"
        )),
        "macaddr8"
    );
    // Already set — unchanged.
    assert_eq!(
        text(&first(
            &mut e,
            "SELECT (macaddr8_set7bit('02:34:56:ab:cd:ef:12:34'::macaddr8))::text"
        )),
        "02:34:56:ab:cd:ef:12:34"
    );
}

#[test]
fn connection_probes_return_null() {
    let mut e = Engine::new();
    for f in &[
        "inet_client_addr()",
        "inet_server_addr()",
        "inet_client_port()",
        "inet_server_port()",
    ] {
        let sql = format!("SELECT {f}");
        assert!(
            matches!(first(&mut e, &sql), spg_storage::Value::Null),
            "SELECT {f} should be NULL (no TCP connection in embedded)"
        );
    }
}

#[test]
fn timeofday_returns_formatted_text() {
    let mut e = Engine::new();
    assert_eq!(
        text(&first(&mut e, "SELECT timeofday()")),
        "Wed Jan 01 00:00:00.000000 2020 UTC"
    );
}

#[test]
fn inet_merge_null_passthrough() {
    let mut e = Engine::new();
    for f in &[
        "inet_merge(NULL::text, '10.0.0.1/8')",
        "macaddr8_set7bit(NULL::text)",
    ] {
        let sql = format!("SELECT {f}");
        assert!(
            matches!(first(&mut e, &sql), spg_storage::Value::Null),
            "SELECT {f} should be NULL"
        );
    }
}
