//! v7.38 (T31) — recursive-CTE `SEARCH { DEPTH | BREADTH } FIRST BY col SET ord`.
//! The ordering key desugars to a typed array (root→node path for DEPTH,
//! `[depth, key]` for BREADTH), so element-wise array ORDER BY reproduces PG's
//! record[] ordering — including multi-digit keys, which the previous
//! honest-error path refused to mis-order. Oracle: live PG 18.4.

use spg_engine::{Engine, QueryResult};

fn seed() -> Engine {
    let mut e = Engine::new();
    e.execute("CREATE TABLE tr(id int, parent int, nm text)")
        .unwrap();
    // A tree where a child id (10) is numerically larger than a deeper node
    // (3), so text ordering of the key would mis-place it.
    e.execute("INSERT INTO tr VALUES (1,NULL,'r'),(2,1,'x'),(10,1,'a'),(3,2,'y'),(20,10,'z')")
        .unwrap();
    e
}

fn ids(e: &mut Engine, sql: &str) -> Vec<i32> {
    match e
        .execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"))
    {
        QueryResult::Rows { rows, .. } => rows
            .iter()
            .map(|r| match &r.values[0] {
                spg_storage::Value::Int(n) => *n,
                other => panic!("expected int id, got {other:?}"),
            })
            .collect(),
        other => panic!("expected rows, got {other:?}"),
    }
}

const REC: &str = "WITH RECURSIVE t(id, parent, nm) AS (\
     SELECT id, parent, nm FROM tr WHERE parent IS NULL \
     UNION ALL \
     SELECT c.id, c.parent, c.nm FROM tr c JOIN t ON c.parent = t.id)";

#[test]
fn depth_first_orders_by_root_to_node_path() {
    let mut e = seed();
    // PG: 1, 2, 3, 10, 20 — the (1,10) subtree comes after (1,2,3) because
    // 2 < 10 numerically, NOT after "10" < "2" text-wise.
    assert_eq!(
        ids(
            &mut e,
            &format!("{REC} SEARCH DEPTH FIRST BY id SET ord SELECT id FROM t ORDER BY ord")
        ),
        [1, 2, 3, 10, 20]
    );
}

#[test]
fn breadth_first_orders_by_depth_then_key() {
    let mut e = seed();
    // PG: 1, 2, 10, 3, 20 — level 0, then level 1 (2, 10), then level 2 (3, 20).
    assert_eq!(
        ids(
            &mut e,
            &format!("{REC} SEARCH BREADTH FIRST BY id SET ord SELECT id FROM t ORDER BY ord")
        ),
        [1, 2, 10, 3, 20]
    );
}

#[test]
fn depth_first_with_a_text_key() {
    let mut e = seed();
    // Order by nm along the path (r → a → z under node 10, then x → y under
    // node 2). PG: 1, 10, 20, 2, 3.
    assert_eq!(
        ids(
            &mut e,
            &format!("{REC} SEARCH DEPTH FIRST BY nm SET ord SELECT id FROM t ORDER BY ord")
        ),
        [1, 10, 20, 2, 3]
    );
}

#[test]
fn multi_column_search_by_errors_honestly() {
    // A record[] key is needed to keep the per-node tuple orderable; SPG can't
    // express it, so this errors rather than mis-ordering.
    let mut e = seed();
    let err = e
        .execute(&format!(
            "{REC} SEARCH DEPTH FIRST BY id, parent SET ord SELECT id FROM t ORDER BY ord"
        ))
        .unwrap_err();
    assert!(
        format!("{err:?}").contains("multiple columns"),
        "got {err:?}"
    );
}
