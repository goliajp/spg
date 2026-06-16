//! `indexed_col IN (lit, …)` index seek (v7.33, mailrs 7.33.0). The seek
//! unions per-literal index lookups instead of a full scan + per-row
//! membership test. These pin that the seek returns EXACTLY the IN-list
//! members (single-table and join paths), and that the shapes that must
//! NOT seed (NOT IN, non-indexed column, non-literal list) stay correct.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn setup() -> Engine {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT, k TEXT, v INT)").unwrap();
    e.execute("CREATE INDEX t_k ON t(k)").unwrap();
    // keys a..f, ids 1..6, plus a duplicate key 'a' (id 7).
    e.execute("INSERT INTO t (id, k, v) VALUES (1,'a',10),(2,'b',20),(3,'c',30),(4,'d',40),(5,'e',50),(6,'f',60),(7,'a',70)")
        .unwrap();
    e
}

fn ids(e: &mut Engine, sql: &str) -> Vec<i32> {
    let rows = match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => rows,
        other => panic!("expected rows, got {other:?}"),
    };
    let mut out: Vec<i32> = rows
        .iter()
        .map(|r| match r.values[0] {
            Value::Int(n) => n,
            ref o => panic!("{o:?}"),
        })
        .collect();
    out.sort_unstable();
    out
}

#[test]
fn single_table_in_list_seek_returns_exact_members() {
    let mut e = setup();
    // k IN ('a','c','e') → ids 1,3,5 plus the duplicate 'a' (id 7).
    assert_eq!(
        ids(&mut e, "SELECT id FROM t WHERE k IN ('a','c','e')"),
        vec![1, 3, 5, 7]
    );
    // A key not present is simply absent; no spurious rows.
    assert_eq!(
        ids(&mut e, "SELECT id FROM t WHERE k IN ('c','zzz')"),
        vec![3]
    );
}

#[test]
fn in_list_seek_with_residual_predicate() {
    // Seek narrows to the IN-list; the residual `v > 25` is re-applied.
    let mut e = setup();
    assert_eq!(
        ids(
            &mut e,
            "SELECT id FROM t WHERE k IN ('a','c','e') AND v > 25"
        ),
        vec![3, 5, 7]
    );
}

#[test]
fn not_in_list_is_not_seeded_but_correct() {
    let mut e = setup();
    // NOT IN must return the complement (ids 1,3,5,7 excluded → 2,4,6).
    assert_eq!(
        ids(&mut e, "SELECT id FROM t WHERE k NOT IN ('a','c','e')"),
        vec![2, 4, 6]
    );
}

#[test]
fn in_list_on_unindexed_column_is_correct() {
    // v has no index → full scan, still correct.
    let mut e = setup();
    assert_eq!(
        ids(&mut e, "SELECT id FROM t WHERE v IN (20,40,60)"),
        vec![2, 4, 6]
    );
}

#[test]
fn join_with_in_list_seek_on_driver() {
    // The mailrs shape: a join whose driver is filtered by an IN-list on
    // its indexed column. The seek must feed the join exactly the members.
    let mut e = Engine::new();
    e.execute("CREATE TABLE m (id INT, thread TEXT, mb INT)")
        .unwrap();
    e.execute("CREATE INDEX m_thread ON m(thread)").unwrap();
    e.execute("CREATE TABLE mb (id INT PRIMARY KEY, addr TEXT)")
        .unwrap();
    e.execute("INSERT INTO m (id, thread, mb) VALUES (1,'t1',1),(2,'t1',1),(3,'t2',1),(4,'t3',1),(5,'t9',1)")
        .unwrap();
    e.execute("INSERT INTO mb (id, addr) VALUES (1,'u@x')")
        .unwrap();
    let rows = match e
        .execute(
            "SELECT m.thread, COUNT(*) FROM m JOIN mb ON m.mb = mb.id \
             WHERE mb.addr='u@x' AND m.thread IN ('t1','t2') GROUP BY m.thread ORDER BY m.thread",
        )
        .unwrap()
    {
        QueryResult::Rows { rows, .. } => rows,
        o => panic!("{o:?}"),
    };
    assert_eq!(rows.len(), 2);
    // t1 has 2 messages, t2 has 1; t3/t9 excluded by the IN-list.
    assert!(
        matches!(rows[0].values[1], Value::BigInt(2)),
        "t1 {:?}",
        rows[0].values[1]
    );
    assert!(
        matches!(rows[1].values[1], Value::BigInt(1)),
        "t2 {:?}",
        rows[1].values[1]
    );
}
