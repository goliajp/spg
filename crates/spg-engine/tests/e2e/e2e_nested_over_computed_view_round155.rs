//! v7.39 (read01 round 155) — nested views over a computed-column view
//! (r154 residual, probe P13/N1-N7). PG composes: a plain projection over
//! a partially-updatable view is itself partially updatable — simple
//! columns take every DML through any number of levels, a re-exported
//! computed column stays readable (WHERE / RETURNING) but never a write
//! target, and the write-target error is attributed to the DEFINING
//! level's view and column name even through an outer rename
//! (`cannot update column "dbl" of view "cv"`, not the outer name).
//! Locked byte-identical against PG 18.4 (r155 live probes).

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

fn errs(e: &mut Engine, sql: &str, want: &str) {
    let m = match e.execute(sql) {
        Err(x) => format!("{x}"),
        Ok(r) => panic!("expected error for {sql}, got {r:?}"),
    };
    assert!(m.contains(want), "{m}");
}

fn setup(e: &mut Engine) {
    e.execute("CREATE TABLE t(id int, v int)").unwrap();
    e.execute("INSERT INTO t VALUES (1,10),(2,20)").unwrap();
    e.execute("CREATE TABLE s(id int, v int)").unwrap();
    e.execute("INSERT INTO s VALUES (1,100),(3,300)").unwrap();
    e.execute("CREATE VIEW cv AS SELECT id, v, v*2 AS dbl FROM t")
        .unwrap();
}

/// N1 + N7 — simple columns through one and two plain levels over the
/// computed view: UPDATE / INSERT / DELETE all work.
#[test]
fn simple_columns_through_nested_levels() {
    let mut e = Engine::new();
    setup(&mut e);
    e.execute("CREATE VIEW o1 AS SELECT id, v FROM cv WHERE v > 0")
        .unwrap();
    assert_eq!(affected(&mut e, "UPDATE o1 SET v = 7 WHERE id = 1"), 1);
    assert_eq!(affected(&mut e, "INSERT INTO o1 VALUES (4, 40)"), 1);
    assert_eq!(affected(&mut e, "DELETE FROM o1 WHERE id = 4"), 1);
    assert_eq!(
        pairs(&mut e, "SELECT id, v FROM t ORDER BY id"),
        vec![(1, 7), (2, 20)]
    );
    e.execute("CREATE VIEW o7 AS SELECT id, v FROM o1").unwrap();
    assert_eq!(affected(&mut e, "UPDATE o7 SET v = 1 WHERE id = 1"), 1);
    assert_eq!(
        pairs(&mut e, "SELECT id, v FROM t ORDER BY id"),
        vec![(1, 1), (2, 20)]
    );
}

/// N2/N2b/N3 — a re-exported computed column: simple writes beside it
/// work; targeting it errors with the DEFINING view's name and column,
/// through a rename too.
#[test]
fn reexported_computed_column_error_attribution() {
    let mut e = Engine::new();
    setup(&mut e);
    e.execute("CREATE VIEW o2 AS SELECT id, dbl FROM cv")
        .unwrap();
    assert_eq!(affected(&mut e, "UPDATE o2 SET id = 9 WHERE id = 2"), 1);
    errs(
        &mut e,
        "UPDATE o2 SET dbl = 5 WHERE id = 1",
        "cannot update column \"dbl\" of view \"cv\"",
    );
    e.execute("CREATE VIEW o3 (a, d) AS SELECT id, dbl FROM cv")
        .unwrap();
    assert_eq!(affected(&mut e, "UPDATE o3 SET a = 8 WHERE a = 9"), 1);
    errs(
        &mut e,
        "UPDATE o3 SET d = 5 WHERE a = 8",
        "cannot update column \"dbl\" of view \"cv\"",
    );
    assert_eq!(
        pairs(&mut e, "SELECT id, v FROM t ORDER BY id"),
        vec![(1, 10), (8, 20)]
    );
}

/// N4/N4b — the outer WHERE reads the computed column, and rows outside
/// it stay invisible to writes.
#[test]
fn outer_where_reads_computed() {
    let mut e = Engine::new();
    setup(&mut e);
    e.execute("CREATE VIEW o4 AS SELECT id, v FROM cv WHERE dbl > 30")
        .unwrap();
    assert_eq!(affected(&mut e, "UPDATE o4 SET v = 99 WHERE id = 2"), 1);
    assert_eq!(affected(&mut e, "UPDATE o4 SET v = 55 WHERE id = 1"), 0);
    assert_eq!(
        pairs(&mut e, "SELECT id, v FROM t ORDER BY id"),
        vec![(1, 10), (2, 99)]
    );
}

/// N5 + N6 — MERGE through the nested-over-computed view, and RETURNING
/// of the re-exported computed column.
#[test]
fn merge_and_returning_through_nested() {
    let mut e = Engine::new();
    setup(&mut e);
    e.execute("CREATE VIEW o1 AS SELECT id, v FROM cv WHERE v > 0")
        .unwrap();
    assert_eq!(
        affected(
            &mut e,
            "MERGE INTO o1 USING s ON o1.id = s.id \
             WHEN MATCHED THEN UPDATE SET v = s.v \
             WHEN NOT MATCHED THEN INSERT VALUES (s.id, s.v)"
        ),
        2
    );
    assert_eq!(
        pairs(&mut e, "SELECT id, v FROM t ORDER BY id"),
        vec![(1, 100), (2, 20), (3, 300)]
    );
    e.execute("CREATE VIEW o2 AS SELECT id, dbl FROM cv")
        .unwrap();
    let r = e
        .execute("UPDATE o2 SET id = 5 WHERE id = 2 RETURNING id, dbl")
        .unwrap();
    match r {
        QueryResult::Rows { columns, rows } => {
            assert_eq!(columns[1].name, "dbl");
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0].values[0], spg_storage::Value::Int(5));
            assert_eq!(rows[0].values[1], spg_storage::Value::Int(40));
        }
        other => panic!("{other:?}"),
    }
}
