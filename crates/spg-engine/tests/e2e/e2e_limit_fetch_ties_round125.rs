//! v7.39 (read01 round 125, Track A — nodeLimit.c 补读) — LIMIT / OFFSET /
//! FETCH FIRST … WITH TIES, locked byte-identical against PG 18.4.
//!
//! Read-driven scan of `src/backend/executor/nodeLimit.c`: no SPG divergence.
//! Pins lock the SQL-standard forms, especially `FETCH FIRST n ROWS WITH TIES`
//! (returns n rows plus every later row whose ORDER BY key ties the n-th row).

use spg_engine::{Engine, QueryResult};

fn count(e: &mut Engine, sql: &str) -> i64 {
    match e.execute(sql).unwrap_or_else(|x| panic!("{sql}: {x:?}")) {
        QueryResult::Rows { rows, .. } => match &rows[0].values[0] {
            spg_storage::Value::BigInt(n) => *n,
            spg_storage::Value::Int(n) => i64::from(*n),
            other => panic!("{sql}: {other:?}"),
        },
        other => panic!("{sql}: {other:?}"),
    }
}

fn text(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap_or_else(|x| panic!("{sql}: {x:?}")) {
        QueryResult::Rows { rows, .. } => match &rows[0].values[0] {
            spg_storage::Value::Null => "NULL".to_string(),
            v => spg_engine::eval::value_to_text(v),
        },
        other => panic!("{sql}: {other:?}"),
    }
}

fn setup(e: &mut Engine) {
    e.execute("CREATE TABLE l (id int, g int)").unwrap();
    e.execute("INSERT INTO l VALUES (1,10),(2,10),(3,20),(4,20),(5,30)")
        .unwrap();
}

#[test]
fn fetch_first_with_ties_includes_boundary_peers() {
    let mut e = Engine::new();
    setup(&mut e);
    // The 1st row's g=10 group has two rows → WITH TIES returns both.
    assert_eq!(
        count(
            &mut e,
            "SELECT count(*) FROM (SELECT id FROM l ORDER BY g FETCH FIRST 1 ROW WITH TIES) q"
        ),
        2
    );
    // The 3rd row's g=20 group adds a 4th row.
    assert_eq!(
        count(
            &mut e,
            "SELECT count(*) FROM (SELECT id FROM l ORDER BY g FETCH FIRST 3 ROWS WITH TIES) q"
        ),
        4
    );
    assert_eq!(
        text(
            &mut e,
            "SELECT string_agg(g::text,'/') FROM (SELECT g FROM l ORDER BY g FETCH FIRST 3 ROWS WITH TIES) q"
        ),
        "10/10/20/20"
    );
}

#[test]
fn standard_limit_offset_fetch_forms() {
    let mut e = Engine::new();
    setup(&mut e);
    assert_eq!(
        text(
            &mut e,
            "SELECT string_agg(id::text,'/') FROM (SELECT id FROM l ORDER BY g OFFSET 1 ROW FETCH NEXT 2 ROWS ONLY) q"
        ),
        "2/3"
    );
    // LIMIT NULL / LIMIT ALL = no limit.
    assert_eq!(
        count(
            &mut e,
            "SELECT count(*) FROM (SELECT id FROM l ORDER BY g LIMIT NULL) q"
        ),
        5
    );
    assert_eq!(
        count(
            &mut e,
            "SELECT count(*) FROM (SELECT id FROM l ORDER BY g LIMIT ALL) q"
        ),
        5
    );
}
