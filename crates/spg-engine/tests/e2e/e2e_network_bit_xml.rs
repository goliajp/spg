//! v7.37.5 ζ-A — network / bit string / XML / "char" / money[]
//! smoke tests.
//!
//! Catalog tags 57..65; wire OIDs 869 (inet) / 650 (cidr) /
//! 829 (macaddr) / 774 (macaddr8) / 1560 (bit) / 1562 (varbit) /
//! 142 (xml) / 18 ("char") / 791 (money[]).
//!
//! Promoted from v7.17.0's Text-backed fallback for inet/cidr/
//! macaddr into first-class typed columns.

use spg_engine::{Engine, QueryResult};
use spg_storage::{DataType, Value};

fn rows(r: QueryResult) -> Vec<Vec<Value>> {
    match r {
        QueryResult::Rows { rows, .. } => rows.into_iter().map(|r| r.values).collect(),
        _ => panic!("expected Rows"),
    }
}

fn col_type(e: &mut Engine, sql: &str) -> DataType {
    let r = e.execute(sql).unwrap();
    let QueryResult::Rows { columns, .. } = r else {
        panic!()
    };
    columns[0].ty
}

#[test]
fn inet_ipv4_round_trip() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT NOT NULL, a INET NOT NULL)")
        .unwrap();
    assert_eq!(col_type(&mut e, "SELECT a FROM t"), DataType::Inet);
    e.execute("INSERT INTO t VALUES (1, '192.168.1.42/24'::inet)")
        .unwrap();
    let r = rows(e.execute("SELECT a FROM t").unwrap());
    let Value::Inet { family, bits, addr } = &r[0][0] else {
        panic!("got {:?}", r[0][0]);
    };
    assert_eq!(*family, 4);
    assert_eq!(*bits, 24);
    assert_eq!(&addr[..4], &[192, 168, 1, 42]);
}

#[test]
fn inet_ipv6_round_trip() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT NOT NULL, a INET NOT NULL)")
        .unwrap();
    e.execute("INSERT INTO t VALUES (1, '2001:db8:0:0:0:0:0:1'::inet)")
        .unwrap();
    let r = rows(e.execute("SELECT a FROM t").unwrap());
    let Value::Inet { family, bits, addr } = &r[0][0] else {
        panic!()
    };
    assert_eq!(*family, 6);
    assert_eq!(*bits, 128);
    assert_eq!(addr[0], 0x20);
    assert_eq!(addr[1], 0x01);
    assert_eq!(addr[15], 0x01);
}

#[test]
fn cidr_round_trip() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT NOT NULL, n CIDR NOT NULL)")
        .unwrap();
    assert_eq!(col_type(&mut e, "SELECT n FROM t"), DataType::Cidr);
    e.execute("INSERT INTO t VALUES (1, '10.0.0.0/8'::cidr)")
        .unwrap();
    let r = rows(e.execute("SELECT n FROM t").unwrap());
    let Value::Cidr { family, bits, .. } = &r[0][0] else {
        panic!()
    };
    assert_eq!(*family, 4);
    assert_eq!(*bits, 8);
}

#[test]
fn macaddr_round_trip_canonical_form() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT NOT NULL, m MACADDR NOT NULL)")
        .unwrap();
    e.execute("INSERT INTO t VALUES (1, '08:00:2b:01:02:03'::macaddr)")
        .unwrap();
    let r = rows(e.execute("SELECT m FROM t").unwrap());
    assert_eq!(
        r[0][0],
        Value::Macaddr([0x08, 0x00, 0x2b, 0x01, 0x02, 0x03])
    );
}

#[test]
fn macaddr8_round_trip_eui64() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT NOT NULL, m MACADDR8 NOT NULL)")
        .unwrap();
    e.execute("INSERT INTO t VALUES (1, '08:00:2b:01:02:03:04:05'::macaddr8)")
        .unwrap();
    let r = rows(e.execute("SELECT m FROM t").unwrap());
    assert_eq!(
        r[0][0],
        Value::Macaddr8([0x08, 0x00, 0x2b, 0x01, 0x02, 0x03, 0x04, 0x05])
    );
}

#[test]
fn bit_string_round_trip() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT NOT NULL, b BIT NOT NULL)")
        .unwrap();
    // 12-bit string `101010111100` → nbits=12, packed BE per byte
    // = [10101011 11000000] = [0xab, 0xc0].
    e.execute("INSERT INTO t VALUES (1, '101010111100'::bit)")
        .unwrap();
    let r = rows(e.execute("SELECT b FROM t").unwrap());
    let Value::BitString { nbits, bytes } = &r[0][0] else {
        panic!()
    };
    assert_eq!(*nbits, 12);
    assert_eq!(bytes, &vec![0xab, 0xc0]);
}

#[test]
fn varbit_round_trip() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT NOT NULL, b VARBIT NOT NULL)")
        .unwrap();
    assert_eq!(col_type(&mut e, "SELECT b FROM t"), DataType::BitVarying);
    e.execute("INSERT INTO t VALUES (1, '11111111'::varbit)")
        .unwrap();
    let r = rows(e.execute("SELECT b FROM t").unwrap());
    let Value::BitString { nbits, bytes } = &r[0][0] else {
        panic!()
    };
    assert_eq!(*nbits, 8);
    assert_eq!(bytes, &vec![0xff]);
}

#[test]
fn xml_round_trip_verbatim() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT NOT NULL, x XML NOT NULL)")
        .unwrap();
    assert_eq!(col_type(&mut e, "SELECT x FROM t"), DataType::Xml);
    let doc = "<root><name>alice</name></root>";
    e.execute(&format!("INSERT INTO t VALUES (1, '{doc}'::xml)"))
        .unwrap();
    let r = rows(e.execute("SELECT x FROM t").unwrap());
    let Value::Xml(s) = &r[0][0] else { panic!() };
    assert_eq!(s, doc);
}

#[test]
fn money_array_round_trip() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT NOT NULL, xs MONEY[] NOT NULL)")
        .unwrap();
    assert_eq!(col_type(&mut e, "SELECT xs FROM t"), DataType::MoneyArray);
    e.execute("INSERT INTO t VALUES (1, ARRAY['$1.00'::money, NULL, '$2.50'::money])")
        .unwrap();
    let r = rows(e.execute("SELECT xs FROM t").unwrap());
    let Value::MoneyArray(items) = &r[0][0] else {
        panic!("got {:?}", r[0][0]);
    };
    assert_eq!(items.len(), 3);
    // PG stores money as i64 cents; `$1.00` = 100, `$2.50` = 250.
    assert_eq!(items[0], Some(100));
    assert_eq!(items[1], None);
    assert_eq!(items[2], Some(250));
}

#[test]
fn inet_cast_to_text_renders_with_mask() {
    let mut e = Engine::new();
    let r = rows(e.execute("SELECT '10.0.0.1/24'::inet::text").unwrap());
    let Value::Text(s) = &r[0][0] else { panic!() };
    assert_eq!(s, "10.0.0.1/24");
}

#[test]
fn macaddr_cast_to_text_renders_canonical() {
    let mut e = Engine::new();
    let r = rows(
        e.execute("SELECT '08:00:2b:01:02:03'::macaddr::text")
            .unwrap(),
    );
    let Value::Text(s) = &r[0][0] else { panic!() };
    assert_eq!(s, "08:00:2b:01:02:03");
}
