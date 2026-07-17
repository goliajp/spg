//! v7.39 (read01 round 154) — partially-updatable views (r148 residual ①).
//! PG keeps a view with computed (expression) columns auto-updatable: the
//! simple columns take every DML, a write TARGETING a computed column
//! errors 0A000 `cannot {insert into|update|merge into} column "c" of view
//! "v"` + `DETAIL: View columns that are not columns of their base relation
//! are not updatable.`, computed columns stay readable (WHERE, RETURNING —
//! post-write value). Aliased projections (`SELECT id AS key …`) are plain
//! renames and fully updatable. SPG's view redirect used to bail on any
//! non-bare-column projection — every one of these was an honest error.
//! Locked byte-identical against PG 18.4 (r154 live probes P1-P13).

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

const DETAIL: &str = "View columns that are not columns of their base relation are not updatable.";

fn setup(e: &mut Engine) {
    e.execute("CREATE TABLE t(id int, v int)").unwrap();
    e.execute("INSERT INTO t VALUES (1,10),(2,20)").unwrap();
    e.execute("CREATE TABLE s(id int, v int)").unwrap();
    e.execute("INSERT INTO s VALUES (1,100),(3,300)").unwrap();
    e.execute("CREATE VIEW cv AS SELECT id, v, v*2 AS dbl FROM t")
        .unwrap();
}

/// P1/P2/P12 — INSERT: simple columns pass (named and short positional),
/// touching the computed column errors (positional full-width and named).
#[test]
fn insert_simple_ok_computed_rejected() {
    let mut e = Engine::new();
    setup(&mut e);
    assert_eq!(affected(&mut e, "INSERT INTO cv (id, v) VALUES (4, 40)"), 1);
    assert_eq!(affected(&mut e, "INSERT INTO cv VALUES (5, 50)"), 1);
    errs(
        &mut e,
        "INSERT INTO cv VALUES (6, 60, 100)",
        "cannot insert into column \"dbl\" of view \"cv\"",
    );
    errs(&mut e, "INSERT INTO cv (id, dbl) VALUES (6, 100)", DETAIL);
    assert_eq!(
        pairs(&mut e, "SELECT id, v FROM t ORDER BY id"),
        vec![(1, 10), (2, 20), (4, 40), (5, 50)]
    );
}

/// P3/P4/P11 — UPDATE: simple column passes (incl. computed read in
/// WHERE), computed target errors.
#[test]
fn update_simple_ok_computed_rejected() {
    let mut e = Engine::new();
    setup(&mut e);
    assert_eq!(affected(&mut e, "UPDATE cv SET v = 11 WHERE id = 1"), 1);
    assert_eq!(affected(&mut e, "UPDATE cv SET v = 5 WHERE dbl > 30"), 1);
    errs(
        &mut e,
        "UPDATE cv SET dbl = 9 WHERE id = 1",
        "cannot update column \"dbl\" of view \"cv\"",
    );
    assert_eq!(
        pairs(&mut e, "SELECT id, v FROM t ORDER BY id"),
        vec![(1, 11), (2, 5)]
    );
}

/// P5/P11b — DELETE through the view works, computed readable in WHERE.
#[test]
fn delete_with_computed_in_where() {
    let mut e = Engine::new();
    setup(&mut e);
    assert_eq!(affected(&mut e, "DELETE FROM cv WHERE dbl = 40"), 1);
    assert_eq!(
        pairs(&mut e, "SELECT id, v FROM t ORDER BY id"),
        vec![(1, 10)]
    );
}

/// P6/P7/P7b — MERGE: simple-column actions pass; positional INSERT
/// hitting the computed slot and SET of a computed column error.
#[test]
fn merge_simple_ok_computed_rejected() {
    let mut e = Engine::new();
    setup(&mut e);
    assert_eq!(
        affected(
            &mut e,
            "MERGE INTO cv USING s ON cv.id = s.id \
             WHEN MATCHED THEN UPDATE SET v = s.v \
             WHEN NOT MATCHED THEN INSERT (id, v) VALUES (s.id, s.v)"
        ),
        2
    );
    assert_eq!(
        pairs(&mut e, "SELECT id, v FROM t ORDER BY id"),
        vec![(1, 100), (2, 20), (3, 300)]
    );
    errs(
        &mut e,
        "MERGE INTO cv USING s ON cv.id = s.id + 100 \
         WHEN NOT MATCHED THEN INSERT VALUES (s.id + 100, s.v, 0)",
        "cannot merge into column \"dbl\" of view \"cv\"",
    );
    errs(
        &mut e,
        "MERGE INTO cv USING s ON cv.id = s.id WHEN MATCHED THEN UPDATE SET dbl = 5",
        DETAIL,
    );
}

/// P8/P9 — RETURNING reads the computed column at its post-write value,
/// for UPDATE and MERGE alike.
#[test]
fn returning_reads_computed_post_write() {
    let mut e = Engine::new();
    setup(&mut e);
    let r = e
        .execute("UPDATE cv SET v = 12 WHERE id = 1 RETURNING id, v, dbl")
        .unwrap();
    match r {
        QueryResult::Rows { columns, rows } => {
            assert_eq!(columns[2].name, "dbl");
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0].values[1], spg_storage::Value::Int(12));
            assert_eq!(rows[0].values[2], spg_storage::Value::Int(24));
        }
        other => panic!("{other:?}"),
    }
    let r = e
        .execute(
            "MERGE INTO cv USING s ON cv.id = s.id \
             WHEN MATCHED AND cv.id = 1 THEN UPDATE SET v = 13 \
             RETURNING merge_action(), cv.id, cv.v, dbl",
        )
        .unwrap();
    match r {
        QueryResult::Rows { columns, rows } => {
            assert_eq!(columns[3].name, "dbl");
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0].values[2], spg_storage::Value::Int(13));
            assert_eq!(rows[0].values[3], spg_storage::Value::Int(26));
        }
        other => panic!("{other:?}"),
    }
}

/// Aliased projection (`SELECT id AS key …`) — a plain rename, fully
/// updatable in PG (r154c probe); SPG used to treat it as the identity
/// and error on the view-side names.
#[test]
fn aliased_column_view_updatable() {
    let mut e = Engine::new();
    setup(&mut e);
    e.execute("CREATE VIEW av AS SELECT id AS key, v FROM t")
        .unwrap();
    assert_eq!(
        affected(&mut e, "INSERT INTO av (key, v) VALUES (7, 70)"),
        1
    );
    assert_eq!(affected(&mut e, "UPDATE av SET key = 9 WHERE key = 1"), 1);
    assert_eq!(affected(&mut e, "INSERT INTO av VALUES (3, 30)"), 1);
    assert_eq!(
        pairs(&mut e, "SELECT id, v FROM t ORDER BY id"),
        vec![(2, 20), (3, 30), (7, 70), (9, 10)]
    );
}
