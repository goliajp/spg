//! v7.38 (read01 U16) — `WITH RECURSIVE … CYCLE col SET mark USING path`.
//! Desugars to a path-array + membership test, matching PG. SEARCH
//! DEPTH/BREADTH FIRST is parsed but errors honestly (its SET column is
//! meant to be ORDER BY'd, which needs typed row ordering SPG lacks).

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn run(e: &mut Engine, sql: &str) -> Vec<Vec<Value<'static>>> {
    match e
        .execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"))
    {
        QueryResult::Rows { rows, .. } => rows.into_iter().map(|r| r.values).collect(),
        _ => panic!("expected rows"),
    }
}

#[test]
fn cycle_detects_and_stops() {
    // 1 → 2 → 3 → 1 is a cycle. PG marks is_cyc on the revisit of 1 and
    // stops expanding it. Live-PG18.4: rows (1,2,f)(2,3,f)(3,1,f)(1,2,t).
    let mut e = Engine::new();
    e.execute("CREATE TABLE g(a int, b int)").unwrap();
    e.execute("INSERT INTO g VALUES(1,2),(2,3),(3,1)").unwrap();
    let out = run(
        &mut e,
        "WITH RECURSIVE t(a,b) AS (\
           SELECT a,b FROM g WHERE a=1 \
           UNION ALL \
           SELECT g.a,g.b FROM g JOIN t ON g.a=t.b\
         ) CYCLE a SET is_cyc USING path \
         SELECT a,b,is_cyc FROM t",
    );
    assert_eq!(out.len(), 4, "cycle must terminate after one revisit");
    let cyc: Vec<bool> = out
        .iter()
        .map(|r| matches!(r[2], Value::Bool(true)))
        .collect();
    assert_eq!(cyc, vec![false, false, false, true]);
    // The last row is the revisited (1,2).
    assert!(matches!(out[3][0], Value::Int(1)) && matches!(out[3][1], Value::Int(2)));
}

#[test]
fn cycle_no_cycle_terminates_normally() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE dag(a int, b int)").unwrap();
    e.execute("INSERT INTO dag VALUES(1,2),(2,3),(3,4)")
        .unwrap();
    let out = run(
        &mut e,
        "WITH RECURSIVE t(a,b) AS (\
           SELECT a,b FROM dag WHERE a=1 \
           UNION ALL \
           SELECT d.a,d.b FROM dag d JOIN t ON d.a=t.b\
         ) CYCLE a SET is_cyc USING path \
         SELECT a,b,is_cyc FROM t",
    );
    // 1→2→3→4, no repeat: three rows, none flagged.
    assert_eq!(out.len(), 3);
    assert!(out.iter().all(|r| matches!(r[2], Value::Bool(false))));
}

#[test]
fn search_by_a_single_column_works() {
    // v7.38 (T31) — a single-column SEARCH BY now desugars to a typed array
    // key that ORDER BY sorts correctly, so this no longer errors. The graph
    // has one edge 1→2, so the recursion yields the single root row. PG18.4:
    // `1 | 2 | {(1)}`.
    let mut e = Engine::new();
    e.execute("CREATE TABLE g(a int, b int)").unwrap();
    e.execute("INSERT INTO g VALUES(1,2)").unwrap();
    let r = e
        .execute(
            "WITH RECURSIVE t(a,b) AS (\
               SELECT a,b FROM g WHERE a=1 \
               UNION ALL \
               SELECT g.a,g.b FROM g JOIN t ON g.a=t.b\
             ) SEARCH DEPTH FIRST BY a SET ord SELECT a, b FROM t ORDER BY ord",
        )
        .unwrap();
    let spg_engine::QueryResult::Rows { rows, .. } = r else {
        panic!("rows")
    };
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].values[0], spg_storage::Value::Int(1));
    assert_eq!(rows[0].values[1], spg_storage::Value::Int(2));
}
