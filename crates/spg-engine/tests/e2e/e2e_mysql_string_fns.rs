//! v7.37.17 (17.6 siblings) — MySQL-compat string functions:
//! locate / instr / substring_index / find_in_set / elt / field /
//! space.

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

fn int(v: &spg_storage::Value<'_>) -> i32 {
    match v {
        spg_storage::Value::Int(n) => *n,
        other => panic!("expected Int, got {other:?}"),
    }
}

fn text(v: &spg_storage::Value<'_>) -> String {
    match v {
        spg_storage::Value::Text(s) => s.to_string(),
        other => panic!("expected Text, got {other:?}"),
    }
}

#[test]
fn locate_mysql_arg_order() {
    let mut e = Engine::new();
    // MySQL doc vector: LOCATE('bar', 'foobarbar') = 4.
    assert_eq!(int(&first(&mut e, "SELECT locate('bar', 'foobarbar')")), 4);
    // 3-arg form starts the search at pos: LOCATE('bar','foobarbar',5)=7.
    assert_eq!(
        int(&first(&mut e, "SELECT locate('bar', 'foobarbar', 5)")),
        7
    );
    assert_eq!(int(&first(&mut e, "SELECT locate('xbar', 'foobar')")), 0);
}

#[test]
fn instr_pg_arg_order() {
    let mut e = Engine::new();
    // INSTR('foobarbar', 'bar') = 4 (haystack first).
    assert_eq!(int(&first(&mut e, "SELECT instr('foobarbar', 'bar')")), 4);
    assert_eq!(int(&first(&mut e, "SELECT instr('foobar', 'zzz')")), 0);
}

#[test]
fn substring_index_both_directions() {
    let mut e = Engine::new();
    // MySQL doc vectors.
    assert_eq!(
        text(&first(
            &mut e,
            "SELECT substring_index('www.mysql.com', '.', 2)"
        )),
        "www.mysql"
    );
    assert_eq!(
        text(&first(
            &mut e,
            "SELECT substring_index('www.mysql.com', '.', -2)"
        )),
        "mysql.com"
    );
    // Count beyond occurrences returns the whole string.
    assert_eq!(
        text(&first(
            &mut e,
            "SELECT substring_index('www.mysql.com', '.', 9)"
        )),
        "www.mysql.com"
    );
}

#[test]
fn find_in_set_comma_list() {
    let mut e = Engine::new();
    // MySQL doc vector: FIND_IN_SET('b','a,b,c,d') = 2.
    assert_eq!(int(&first(&mut e, "SELECT find_in_set('b', 'a,b,c,d')")), 2);
    assert_eq!(int(&first(&mut e, "SELECT find_in_set('z', 'a,b,c,d')")), 0);
}

#[test]
fn elt_and_field() {
    let mut e = Engine::new();
    // MySQL doc vector: ELT(1, 'Aa', 'Bb', 'Cc') = 'Aa'.
    assert_eq!(
        text(&first(&mut e, "SELECT elt(2, 'Aa', 'Bb', 'Cc')")),
        "Bb"
    );
    // Out of range → NULL.
    assert!(matches!(
        first(&mut e, "SELECT elt(4, 'Aa', 'Bb', 'Cc')"),
        spg_storage::Value::Null
    ));
    // FIELD('Bb', 'Aa', 'Bb', 'Cc') = 2.
    assert_eq!(
        int(&first(&mut e, "SELECT field('Bb', 'Aa', 'Bb', 'Cc')")),
        2
    );
    assert_eq!(int(&first(&mut e, "SELECT field('Zz', 'Aa', 'Bb')")), 0);
}

#[test]
fn space_repeats() {
    let mut e = Engine::new();
    assert_eq!(text(&first(&mut e, "SELECT space(3)")), "   ");
    assert_eq!(text(&first(&mut e, "SELECT space(0)")), "");
    assert_eq!(text(&first(&mut e, "SELECT space(-5)")), "");
}

#[test]
fn mysql_string_null_passthrough() {
    let mut e = Engine::new();
    for f in &[
        "locate(NULL::text, 'x')",
        "instr('x', NULL::text)",
        "substring_index(NULL::text, '.', 1)",
        "find_in_set(NULL::text, 'a,b')",
        "elt(NULL::int, 'a')",
        "space(NULL::int)",
    ] {
        let sql = format!("SELECT {f}");
        assert!(
            matches!(first(&mut e, &sql), spg_storage::Value::Null),
            "SELECT {f} should be NULL"
        );
    }
}
