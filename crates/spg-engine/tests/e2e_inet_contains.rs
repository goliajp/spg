//! v7.17.0 Phase 3.P0-47 — INET / CIDR containment + overlap operators.
//!
//! Locks the PG-canonical surface for:
//!   * `<<`  — strictly contained-in (LHS inside RHS, no equality)
//!   * `<<=` — contained-in-or-equal
//!   * `>>`  — strict contains (LHS is strict supernet of RHS)
//!   * `>>=` — contains-or-equal
//!   * `&&`  — overlap (any shared addresses)
//!
//! INET / CIDR are stored as Text in SPG (Phase 7 design); these
//! operators parse the textual `addr[/mask]` form and compare
//! networks bit-by-bit.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn bool_of(r: QueryResult) -> Value {
    match r {
        QueryResult::Rows { rows, .. } => rows[0].values[0].clone(),
        _ => panic!("expected rows"),
    }
}

#[test]
fn supernet_contains_member_ipv4() {
    let mut e = Engine::new();
    let r = e
        .execute("SELECT '10.0.0.0/8'::inet >>= '10.1.2.3'::inet")
        .unwrap();
    assert_eq!(bool_of(r), Value::Bool(true));
}

#[test]
fn supernet_does_not_contain_outside() {
    let mut e = Engine::new();
    let r = e
        .execute("SELECT '10.0.0.0/8'::inet >>= '11.0.0.1'::inet")
        .unwrap();
    assert_eq!(bool_of(r), Value::Bool(false));
}

#[test]
fn member_contained_by_supernet_ipv4() {
    let mut e = Engine::new();
    let r = e
        .execute("SELECT '10.0.0.1'::inet <<= '10.0.0.0/8'::inet")
        .unwrap();
    assert_eq!(bool_of(r), Value::Bool(true));
}

#[test]
fn equal_networks_contained_by_eq_true() {
    let mut e = Engine::new();
    let r = e
        .execute("SELECT '10.0.0.0/8'::inet <<= '10.0.0.0/8'::inet")
        .unwrap();
    assert_eq!(bool_of(r), Value::Bool(true));
}

#[test]
fn equal_networks_strict_contained_by_false() {
    let mut e = Engine::new();
    let r = e
        .execute("SELECT '10.0.0.0/8'::inet << '10.0.0.0/8'::inet")
        .unwrap();
    assert_eq!(bool_of(r), Value::Bool(false));
}

#[test]
fn equal_networks_strict_contains_false() {
    let mut e = Engine::new();
    let r = e
        .execute("SELECT '10.0.0.0/8'::inet >> '10.0.0.0/8'::inet")
        .unwrap();
    assert_eq!(bool_of(r), Value::Bool(false));
}

#[test]
fn strict_supernet_strict_contains_subnet() {
    let mut e = Engine::new();
    let r = e
        .execute("SELECT '10.0.0.0/8'::inet >> '10.1.0.0/16'::inet")
        .unwrap();
    assert_eq!(bool_of(r), Value::Bool(true));
}

#[test]
fn overlap_subnet_pair_true() {
    let mut e = Engine::new();
    let r = e
        .execute("SELECT '10.0.0.0/8'::inet && '10.1.0.0/16'::inet")
        .unwrap();
    assert_eq!(bool_of(r), Value::Bool(true));
}

#[test]
fn overlap_disjoint_subnets_false() {
    let mut e = Engine::new();
    let r = e
        .execute("SELECT '10.0.0.0/8'::inet && '11.0.0.0/8'::inet")
        .unwrap();
    assert_eq!(bool_of(r), Value::Bool(false));
}

#[test]
fn host_address_implicit_full_mask_contained() {
    // No explicit mask → /32 for IPv4. So 10.0.0.5 ⊆ 10.0.0.0/8 holds.
    let mut e = Engine::new();
    let r = e
        .execute("SELECT '10.0.0.5'::inet <<= '10.0.0.0/8'::inet")
        .unwrap();
    assert_eq!(bool_of(r), Value::Bool(true));
}

#[test]
fn ipv6_supernet_contains_member() {
    let mut e = Engine::new();
    let r = e
        .execute("SELECT '2001:db8::/32'::inet >>= '2001:db8:1::1'::inet")
        .unwrap();
    assert_eq!(bool_of(r), Value::Bool(true));
}

#[test]
fn null_propagates() {
    let mut e = Engine::new();
    let r = e.execute("SELECT NULL::inet >>= '10.0.0.1'::inet").unwrap();
    assert_eq!(bool_of(r), Value::Null);
    let r = e
        .execute("SELECT '10.0.0.0/8'::inet && NULL::inet")
        .unwrap();
    assert_eq!(bool_of(r), Value::Null);
}

#[test]
fn mixed_family_overlap_false() {
    // IPv4 and IPv6 cannot overlap.
    let mut e = Engine::new();
    let r = e
        .execute("SELECT '10.0.0.0/8'::inet && '2001:db8::/32'::inet")
        .unwrap();
    assert_eq!(bool_of(r), Value::Bool(false));
}
