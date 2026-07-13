//! v7.37.17 (17.6 siblings) — INET completions: family / netmask /
//! hostmask / broadcast / inet_same_family.

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

fn row(e: &mut Engine, sql: &str) -> Vec<String> {
    let r = e
        .execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    let QueryResult::Rows { rows, .. } = r else {
        panic!("expected Rows");
    };
    rows[0]
        .values
        .iter()
        .map(spg_engine::eval::value_to_text)
        .collect()
}

fn text(v: &spg_storage::Value<'_>) -> String {
    match v {
        spg_storage::Value::Text(s) => s.to_string(),
        other => panic!("expected Text, got {other:?}"),
    }
}

#[test]
fn family_v4_and_v6() {
    let mut e = Engine::new();
    match first(&mut e, "SELECT family('192.168.1.1/24')") {
        spg_storage::Value::Int(4) => {}
        other => panic!("got {other:?}"),
    }
    match first(&mut e, "SELECT family('::1')") {
        spg_storage::Value::Int(6) => {}
        other => panic!("got {other:?}"),
    }
}

#[test]
fn netmask_from_prefix() {
    let mut e = Engine::new();
    assert_eq!(
        text(&first(&mut e, "SELECT netmask('192.168.1.0/24')")),
        "255.255.255.0"
    );
    assert_eq!(
        text(&first(&mut e, "SELECT netmask('10.0.0.0/8')")),
        "255.0.0.0"
    );
    assert_eq!(
        text(&first(&mut e, "SELECT netmask('10.1.2.3/32')")),
        "255.255.255.255"
    );
}

#[test]
fn hostmask_complement() {
    let mut e = Engine::new();
    assert_eq!(
        text(&first(&mut e, "SELECT hostmask('192.168.1.0/24')")),
        "0.0.0.255"
    );
    assert_eq!(
        text(&first(&mut e, "SELECT hostmask('10.0.0.0/8')")),
        "0.255.255.255"
    );
}

#[test]
fn broadcast_sets_host_bits() {
    let mut e = Engine::new();
    assert_eq!(
        text(&first(&mut e, "SELECT broadcast('192.168.1.0/24')")),
        "192.168.1.255/24"
    );
    assert_eq!(
        text(&first(&mut e, "SELECT broadcast('10.0.0.0/8')")),
        "10.255.255.255/8"
    );
}

#[test]
fn same_family_checks() {
    let mut e = Engine::new();
    match first(&mut e, "SELECT inet_same_family('192.168.1.1', '10.0.0.1')") {
        spg_storage::Value::Bool(true) => {}
        other => panic!("got {other:?}"),
    }
    match first(&mut e, "SELECT inet_same_family('192.168.1.1', '::1')") {
        spg_storage::Value::Bool(false) => {}
        other => panic!("got {other:?}"),
    }
}

#[test]
fn inet_family_null_passthrough() {
    let mut e = Engine::new();
    for f in &[
        "family(NULL::text)",
        "netmask(NULL::text)",
        "hostmask(NULL::text)",
        "broadcast(NULL::text)",
        "inet_same_family(NULL::text, '::1')",
    ] {
        let sql = format!("SELECT {f}");
        assert!(
            matches!(first(&mut e, &sql), spg_storage::Value::Null),
            "SELECT {f} should be NULL"
        );
    }
}

#[test]
fn abbrev_inet_vs_cidr_and_same_family_values() {
    // abbrev(inet) keeps the full canonical text; abbrev(cidr) drops the
    // octets past the prefix. inet_same_family accepts real inet/cidr
    // values (not just text). All live-PG18.4-verified.
    let mut e = Engine::new();
    // inet: full form.
    assert_eq!(
        text(&first(&mut e, "SELECT abbrev(inet '192.168.1.0/24')")),
        "192.168.1.0/24"
    );
    assert_eq!(
        text(&first(&mut e, "SELECT abbrev(inet '192.168.1.5')")),
        "192.168.1.5"
    );
    // cidr: abbreviated.
    assert_eq!(
        text(&first(&mut e, "SELECT abbrev(cidr '192.168.1.0/24')")),
        "192.168.1/24"
    );
    assert_eq!(
        text(&first(&mut e, "SELECT abbrev(cidr '10.0.0.0/8')")),
        "10/8"
    );
    // inet_same_family over inet/cidr values.
    assert_eq!(
        first(
            &mut e,
            "SELECT inet_same_family(inet '192.168.1.1', inet '10.0.0.1')"
        ),
        spg_storage::Value::Bool(true)
    );
    assert_eq!(
        first(
            &mut e,
            "SELECT inet_same_family(inet '192.168.1.1', inet '::1')"
        ),
        spg_storage::Value::Bool(false)
    );
    assert_eq!(
        first(
            &mut e,
            "SELECT inet_same_family(cidr '192.168.1.0/24', inet '10.0.0.1')"
        ),
        spg_storage::Value::Bool(true)
    );
}

// v7.39 (read01 inet_cidr_ntop.c / inet_net_pton.c) — abbreviated CIDR
// input forms, host-bit validation, cidr set_masklen zeroing, text(inet)
// mask, and the typed inet_merge. All values byte-locked vs PG18.
#[test]
fn cidr_abbreviated_input_and_validation() {
    let mut e = Engine::new();
    assert_eq!(
        row(&mut e, "SELECT cidr '10/8', cidr '10.5/16', cidr '10.5.3/24', cidr '128.1', cidr '192.5.5.240/28'"),
        vec!["10.0.0.0/8", "10.5.0.0/16", "10.5.3.0/24", "128.1.0.0/16", "192.5.5.240/28"]
    );
    // Host bits right of the mask are rejected (PG's dedicated error).
    let err = e.execute("SELECT cidr '10.1.2.3/8'").unwrap_err();
    assert!(
        format!("{err}").contains("invalid cidr value: \"10.1.2.3/8\""),
        "{err}"
    );
}

#[test]
fn set_masklen_cidr_zeroes_host_bits() {
    let mut e = Engine::new();
    assert_eq!(
        row(
            &mut e,
            "SELECT set_masklen(inet '192.168.1.5/24', 16), set_masklen(cidr '10.1.0.0/16', 8)"
        ),
        vec!["192.168.1.5/16", "10.0.0.0/8"]
    );
}

#[test]
fn text_of_inet_carries_full_mask() {
    let mut e = Engine::new();
    assert_eq!(
        row(&mut e, "SELECT text(inet '192.168.1.5'), text(cidr '10/8')"),
        vec!["192.168.1.5/32", "10.0.0.0/8"]
    );
}

#[test]
fn inet_merge_and_same_family_typed() {
    let mut e = Engine::new();
    assert_eq!(
        row(
            &mut e,
            "SELECT inet_merge(inet '192.168.1.5/24', inet '192.168.2.5/24'), \
             inet_same_family(inet '192.168.1.5', inet '::1')"
        ),
        vec!["192.168.0.0/22", "false"]
    );
}
