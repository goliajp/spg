//! v7.38 (read01) — `pg_typeof` reports SPG's remaining scalar value types
//! instead of "unknown" (drivers and ORMs read "unknown" as "no type"), and a
//! bare `varchar` (no typmod) holds a string of any length. Every expected
//! name is from live PG18.4.

use spg_engine::{Engine, QueryResult};

fn one(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err:?}")) {
        QueryResult::Rows { rows, .. } => match &rows[0].values[0] {
            spg_storage::Value::Text(s) => s.to_string(),
            other => panic!("{sql}: expected Text, got {other:?}"),
        },
        other => panic!("{sql}: expected Rows, got {other:?}"),
    }
}

#[test]
fn pg_typeof_names_the_network_and_geometric_types() {
    let mut e = Engine::new();
    for (expr, want) in [
        ("'$10'::money", "money"),
        ("'192.168.1.1'::inet", "inet"),
        ("'192.168.1.0/24'::cidr", "cidr"),
        ("'08:00:2b:01:02:03'::macaddr", "macaddr"),
        ("'08:00:2b:01:02:03:04:05'::macaddr8", "macaddr8"),
        ("'<a/>'::xml", "xml"),
        ("'(1,2)'::point", "point"),
        ("'((0,0),(1,1))'::box", "box"),
        ("'<(0,0),1>'::circle", "circle"),
        ("'[(0,0),(1,1)]'::lseg", "lseg"),
        ("'{1,2,3}'::line", "line"),
        ("'((0,0),(1,1),(2,0))'::polygon", "polygon"),
        ("'[(0,0),(1,1)]'::path", "path"),
    ] {
        assert_eq!(one(&mut e, &format!("SELECT pg_typeof({expr})::text")), want, "{expr}");
    }
}

#[test]
fn pg_typeof_names_ranges_and_records() {
    let mut e = Engine::new();
    assert_eq!(one(&mut e, "SELECT pg_typeof(int4range(1,5))::text"), "int4range");
    assert_eq!(one(&mut e, "SELECT pg_typeof(int8range(1,5))::text"), "int8range");
    assert_eq!(one(&mut e, "SELECT pg_typeof(numrange(1,5))::text"), "numrange");
    assert_eq!(one(&mut e, "SELECT pg_typeof(ROW(1,'a'))::text"), "record");
    assert_eq!(one(&mut e, "SELECT pg_typeof('a'::char(3))::text"), "character");
}

#[test]
fn bare_varchar_has_no_length_limit() {
    // A bare `varchar` (no typmod) is modelled as Varchar(0); it used to read
    // as VARCHAR(0) and reject every non-empty string.
    let mut e = Engine::new();
    assert_eq!(one(&mut e, "SELECT ('a'::varchar)::text"), "a");
    assert_eq!(one(&mut e, "SELECT ('abcdef'::varchar)::text"), "abcdef");
    // An explicit typmod still truncates on cast (PG semantics).
    assert_eq!(one(&mut e, "SELECT ('abcd'::varchar(2))::text"), "ab");
    // GREATEST over a varchar and a text no longer errors.
    assert_eq!(one(&mut e, "SELECT (GREATEST('a'::varchar, 'bb'::text))::text"), "bb");
    // A varchar(n) COLUMN still rejects an over-long value on assignment.
    e.execute("CREATE TABLE vt(a varchar(2))").unwrap();
    assert!(e.execute("INSERT INTO vt VALUES ('abcd')").is_err());
}
