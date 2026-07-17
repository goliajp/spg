//! v7.39 (read01 round 124, Track A — nodeAgg.c 补读) — GROUPING SETS / ROLLUP /
//! CUBE + GROUPING() bitmask, locked byte-identical against PG 18.4.
//!
//! Read-driven scan of `src/backend/executor/nodeAgg.c`. The working grouping-set
//! behaviors are locked here; the one gap found — `GROUPING()` inside ORDER BY —
//! is root-caused and deferred (see dir-executor/nodeAgg.c-full.md): the
//! grouping-set expansion rewrites GROUPING() in SELECT/HAVING but not order_by,
//! and the UNION-ALL model needs the mask materialized as a hidden column.

use spg_engine::{Engine, QueryResult};

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
    e.execute("CREATE TABLE t (a text, b int, v int)").unwrap();
    e.execute("INSERT INTO t VALUES ('x',1,10),('x',2,20),('y',1,30)")
        .unwrap();
}

#[test]
fn rollup_and_cube() {
    let mut e = Engine::new();
    setup(&mut e);
    assert_eq!(
        text(
            &mut e,
            "SELECT string_agg(coalesce(a,'*')||'/'||coalesce(b::text,'*')||'/'||s, ',') \
                      FROM (SELECT a,b,sum(v) s FROM t GROUP BY ROLLUP(a,b) ORDER BY a NULLS LAST, b NULLS LAST) q"
        ),
        "x/1/10,x/2/20,x/*/30,y/1/30,y/*/30,*/*/60"
    );
    assert_eq!(
        text(
            &mut e,
            "SELECT string_agg(coalesce(a,'*')||'/'||coalesce(b::text,'*')||'/'||s, ',') \
                      FROM (SELECT a,b,sum(v) s FROM t GROUP BY CUBE(a,b) ORDER BY a NULLS LAST, b NULLS LAST) q"
        ),
        "x/1/10,x/2/20,x/*/30,y/1/30,y/*/30,*/1/40,*/2/20,*/*/60"
    );
}

#[test]
fn grouping_function_bitmask_single_arg() {
    let mut e = Engine::new();
    setup(&mut e);
    // GROUPING(a): 0 when a is grouped, 1 when a is aggregated (the total row).
    assert_eq!(
        text(
            &mut e,
            "SELECT string_agg(g::text||':'||coalesce(a,'*')||':'||s, ',') \
                      FROM (SELECT GROUPING(a) g, a, sum(v) s FROM t GROUP BY GROUPING SETS ((a),()) ORDER BY a NULLS LAST) q"
        ),
        "0:x:30,0:y:30,1:*:60"
    );
}

#[test]
fn grouping_function_bitmask_multi_arg() {
    let mut e = Engine::new();
    setup(&mut e);
    // GROUPING(a,b): (a,b)→0, (a)→1 (b dropped), (b)→2 (a dropped), ()→3.
    assert_eq!(
        text(
            &mut e,
            "SELECT string_agg(g::text||':'||coalesce(a,'*')||coalesce(b::text,'*')||':'||s, ',') \
                      FROM (SELECT GROUPING(a,b) g, a, b, sum(v) s FROM t GROUP BY GROUPING SETS ((a,b),(a),(b),()) ORDER BY a NULLS LAST, b NULLS LAST) q"
        ),
        "0:x1:10,0:x2:20,1:x*:30,0:y1:30,1:y*:30,2:*1:40,2:*2:20,3:**:60"
    );
}
