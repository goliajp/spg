//! v7.39 (read01 round 148, PG17) — MERGE INTO an auto-updatable view.
//! The merge rewrites to the base table: view column names map to base
//! columns (qualified by the view's alias), the view's WHERE narrows which
//! target rows the merge can see (matched AND not-matched-by-source alike),
//! and a positional INSERT follows the VIEW's column order. Locked
//! byte-identical against PG 18.4. SPG previously reported "relation does
//! not exist" for any MERGE INTO a view.

use spg_engine::{Engine, QueryResult};

fn pairs(e: &mut Engine, sql: &str) -> Vec<(i32, i32)> {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => rows
            .iter()
            .map(|r| match (&r.values[0], &r.values[1]) {
                (spg_storage::Value::Int(a), spg_storage::Value::Int(b)) => (*a, *b),
                other => panic!("{other:?}"),
            })
            .collect(),
        other => panic!("{other:?}"),
    }
}

fn affected(e: &mut Engine, sql: &str) -> usize {
    match e.execute(sql).unwrap() {
        QueryResult::CommandOk { affected, .. } => affected,
        other => panic!("{other:?}"),
    }
}

fn setup(e: &mut Engine) {
    e.execute("CREATE TABLE t(id int, v int)").unwrap();
    e.execute("INSERT INTO t VALUES (1,10),(2,20)").unwrap();
    e.execute("CREATE TABLE s(id int, v int)").unwrap();
    e.execute("INSERT INTO s VALUES (1,100),(3,300)").unwrap();
}

#[test]
fn simple_view_update_and_insert() {
    let mut e = Engine::new();
    setup(&mut e);
    e.execute("CREATE VIEW v1 AS SELECT id, v FROM t").unwrap();
    assert_eq!(
        affected(&mut e, "MERGE INTO v1 USING s ON v1.id = s.id WHEN MATCHED THEN UPDATE SET v = s.v WHEN NOT MATCHED THEN INSERT VALUES (s.id, s.v)"),
        2
    );
    assert_eq!(
        pairs(&mut e, "SELECT id, v FROM t ORDER BY id"),
        vec![(1, 100), (2, 20), (3, 300)]
    );
}

#[test]
fn where_view_narrows_matched_set() {
    let mut e = Engine::new();
    setup(&mut e);
    e.execute("INSERT INTO t VALUES (3,500)").unwrap();
    // View hides (3,500); s matches ids {1,3} but only id=1 is visible.
    e.execute("CREATE VIEW v2 AS SELECT id, v FROM t WHERE v < 150").unwrap();
    assert_eq!(
        affected(&mut e, "MERGE INTO v2 USING s ON v2.id = s.id WHEN MATCHED THEN UPDATE SET v = 999"),
        1
    );
    assert_eq!(
        pairs(&mut e, "SELECT id, v FROM t ORDER BY id"),
        vec![(1, 999), (2, 20), (3, 500)]
    );
}

#[test]
fn where_view_narrows_by_source_set() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t(id int, v int)").unwrap();
    e.execute("INSERT INTO t VALUES (1,10),(2,20),(3,500)").unwrap();
    e.execute("CREATE TABLE s(id int, v int)").unwrap();
    e.execute("INSERT INTO s VALUES (1,100)").unwrap();
    e.execute("CREATE VIEW vw AS SELECT id, v FROM t WHERE v < 150").unwrap();
    // Visible = {1,2}; id=1 matches → BY SOURCE deletes only id=2. The
    // out-of-view (3,500) is untouched even though no source row matches it.
    assert_eq!(
        affected(&mut e, "MERGE INTO vw USING s ON vw.id = s.id WHEN NOT MATCHED BY SOURCE THEN DELETE"),
        1
    );
    assert_eq!(
        pairs(&mut e, "SELECT id, v FROM t ORDER BY id"),
        vec![(1, 10), (3, 500)]
    );
}

#[test]
fn renamed_columns_view() {
    let mut e = Engine::new();
    setup(&mut e);
    e.execute("CREATE VIEW v3(a, b) AS SELECT id, v FROM t").unwrap();
    // Qualified refs in ON / condition and the bare SET column all remap.
    assert_eq!(
        affected(&mut e, "MERGE INTO v3 USING s ON v3.a = s.id WHEN MATCHED AND v3.b = 10 THEN UPDATE SET b = 7"),
        1
    );
    assert_eq!(
        pairs(&mut e, "SELECT id, v FROM t ORDER BY id"),
        vec![(1, 7), (2, 20)]
    );
    // Positional INSERT follows the view's column order (a=id, b=v).
    assert_eq!(
        affected(&mut e, "MERGE INTO v3 USING s ON v3.a = s.id WHEN NOT MATCHED THEN INSERT VALUES (s.id, s.v)"),
        1
    );
    assert_eq!(
        pairs(&mut e, "SELECT id, v FROM t ORDER BY id"),
        vec![(1, 7), (2, 20), (3, 300)]
    );
}

#[test]
fn non_updatable_view_still_errors() {
    // v7.39 (round 154) — the r148 residual is closed: a computed-column
    // view IS partially updatable (PG 17); merging into its SIMPLE columns
    // works, targeting the computed column errors. An aggregate view stays
    // non-updatable (honest error), pinned in e2e_computed_col_view_dml_
    // round154 and e2e_view_auto_updatable.
    let mut e = Engine::new();
    setup(&mut e);
    e.execute("CREATE VIEW v4 AS SELECT id, v, v*2 AS d FROM t").unwrap();
    assert_eq!(
        affected(&mut e, "MERGE INTO v4 USING s ON v4.id = s.id WHEN MATCHED THEN UPDATE SET v = 1"),
        1
    );
    assert!(e
        .execute("MERGE INTO v4 USING s ON v4.id = s.id WHEN MATCHED THEN UPDATE SET d = 1")
        .is_err());
}
