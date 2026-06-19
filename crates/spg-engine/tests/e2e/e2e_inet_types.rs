//! v7.17.0 Phase 7 — INET / CIDR / MACADDR text-backed types.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn rows(r: QueryResult) -> Vec<Vec<Value>> {
    match r {
        QueryResult::Rows { rows, .. } => rows.into_iter().map(|r| r.values).collect(),
        _ => panic!("expected rows"),
    }
}

#[test]
fn inet_column_type_accepted() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE peers (id INT NOT NULL, addr INET NOT NULL)")
        .unwrap();
    e.execute("INSERT INTO peers VALUES (1, '192.168.1.1')")
        .unwrap();
    let r = rows(e.execute("SELECT addr FROM peers").unwrap());
    // v7.37.5 ζ-A — INET is a first-class typed Value::Inet now,
    // not Value::Text. The text form survives via the
    // pgwire / CAST(addr AS TEXT) paths.
    assert!(matches!(r[0][0], Value::Inet { .. }), "got {:?}", r[0][0]);
}

#[test]
fn cidr_column_type_accepted() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE nets (id INT NOT NULL, net CIDR NOT NULL)")
        .unwrap();
    e.execute("INSERT INTO nets VALUES (1, '10.0.0.0/8')")
        .unwrap();
    let r = rows(e.execute("SELECT net FROM nets").unwrap());
    assert!(matches!(r[0][0], Value::Cidr { .. }), "got {:?}", r[0][0]);
}

#[test]
fn macaddr_column_type_accepted() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE hw (id INT NOT NULL, mac MACADDR NOT NULL)")
        .unwrap();
    e.execute("INSERT INTO hw VALUES (1, '00:50:56:c0:00:01')")
        .unwrap();
    let r = rows(e.execute("SELECT mac FROM hw").unwrap());
    assert!(matches!(r[0][0], Value::Macaddr(_)), "got {:?}", r[0][0]);
}

#[test]
fn host_helper_strips_mask() {
    let mut e = Engine::new();
    let r = rows(e.execute("SELECT host('192.168.1.1/24')").unwrap());
    assert_eq!(r[0][0], Value::Text("192.168.1.1".into()));
}

#[test]
fn host_helper_no_mask_passthrough() {
    let mut e = Engine::new();
    let r = rows(e.execute("SELECT host('10.0.0.5')").unwrap());
    assert_eq!(r[0][0], Value::Text("10.0.0.5".into()));
}

#[test]
fn network_helper_masks_octets() {
    let mut e = Engine::new();
    let r = rows(e.execute("SELECT network('192.168.42.1/24')").unwrap());
    assert_eq!(r[0][0], Value::Text("192.168.42.0/24".into()));
    let r = rows(e.execute("SELECT network('10.5.6.7/8')").unwrap());
    assert_eq!(r[0][0], Value::Text("10.0.0.0/8".into()));
}

#[test]
fn masklen_helper_returns_mask_bits() {
    let mut e = Engine::new();
    let r = rows(e.execute("SELECT masklen('192.168.1.1/24')").unwrap());
    assert_eq!(r[0][0], Value::Int(24));
    let r = rows(e.execute("SELECT masklen('10.0.0.0')").unwrap());
    assert_eq!(r[0][0], Value::Int(32));
}

#[test]
fn null_inet_helpers_propagate() {
    let mut e = Engine::new();
    let r = rows(e.execute("SELECT host(NULL::TEXT)").unwrap());
    assert_eq!(r[0][0], Value::Null);
}
