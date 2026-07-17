//! v7.39 (read01 round 152) — closes two round-148 residuals plus a
//! probe-revealed pre-existing gap:
//! 1. WITH CHECK OPTION enforced through MERGE: every row an UPDATE or
//!    INSERT action produces must satisfy the view chain's quals
//!    (44000, `new row violates check option for view "v"` + failing-row
//!    DETAIL, nothing applied); DELETE actions are exempt.
//! 2. MERGE RETURNING through a column-renamed view: view-column refs
//!    remap to base columns, the output keeps the VIEW column names,
//!    and `v.*` expands to the view's columns.
//! 3. (probe P6) a LOWER view carrying its own check option enforces
//!    even when the WRITTEN view has none — for MERGE, UPDATE and
//!    INSERT alike. SPG's gate only armed on the written view's option.
//! Locked byte-identical against PG 18.4 (r152 live probes).

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
    e.execute("INSERT INTO s VALUES (1,200),(3,30)").unwrap();
    e.execute("CREATE VIEW vc AS SELECT id, v FROM t WHERE v < 100 WITH CHECK OPTION")
        .unwrap();
}

/// P1 + P2 — violating UPDATE / INSERT actions error with PG's 44000
/// message and failing-row DETAIL; the table stays untouched.
#[test]
fn merge_check_option_violations() {
    let mut e = Engine::new();
    setup(&mut e);
    errs(
        &mut e,
        "MERGE INTO vc USING s ON vc.id = s.id WHEN MATCHED THEN UPDATE SET v = s.v",
        "new row violates check option for view \"vc\" DETAIL: Failing row contains (1, 200).",
    );
    errs(
        &mut e,
        "MERGE INTO vc USING s ON vc.id = s.id WHEN NOT MATCHED THEN INSERT VALUES (s.id, 500)",
        "new row violates check option for view \"vc\" DETAIL: Failing row contains (3, 500).",
    );
    assert_eq!(
        pairs(&mut e, "SELECT id, v FROM t ORDER BY id"),
        vec![(1, 10), (2, 20)]
    );
}

/// P3 + P4 — conforming actions pass; DELETE actions are exempt from
/// the check.
#[test]
fn merge_check_option_conforming_and_delete_exempt() {
    let mut e = Engine::new();
    setup(&mut e);
    assert_eq!(
        affected(
            &mut e,
            "MERGE INTO vc USING s ON vc.id = s.id \
             WHEN MATCHED THEN UPDATE SET v = 99 \
             WHEN NOT MATCHED THEN INSERT VALUES (s.id, 30)"
        ),
        2
    );
    assert_eq!(
        pairs(&mut e, "SELECT id, v FROM t ORDER BY id"),
        vec![(1, 99), (2, 20), (3, 30)]
    );
    assert_eq!(
        affected(
            &mut e,
            "MERGE INTO vc USING s ON vc.id = s.id \
             WHEN MATCHED AND vc.id = 1 THEN DELETE"
        ),
        1
    );
    assert_eq!(
        pairs(&mut e, "SELECT id, v FROM t ORDER BY id"),
        vec![(2, 20), (3, 30)]
    );
}

/// P5 + P5b — MERGE RETURNING through a column-renamed view: view names
/// on the output, refs remap (bare, view-qualified, and `v.*`).
#[test]
fn merge_returning_via_renamed_view() {
    let mut e = Engine::new();
    setup(&mut e);
    e.execute("INSERT INTO t VALUES (3,30)").unwrap();
    e.execute("CREATE VIEW rv(a, b) AS SELECT id, v FROM t")
        .unwrap();
    let r = e
        .execute(
            "MERGE INTO rv USING s ON rv.a = s.id \
             WHEN MATCHED AND rv.a = 3 THEN UPDATE SET b = s.v + 1 \
             RETURNING merge_action(), a, b",
        )
        .unwrap();
    match r {
        QueryResult::Rows { columns, rows } => {
            assert_eq!(columns[1].name, "a");
            assert_eq!(columns[2].name, "b");
            assert_eq!(rows.len(), 1);
            assert_eq!(format!("{:?}", rows[0].values[0]), "Text(\"UPDATE\")");
            assert_eq!(rows[0].values[1], spg_storage::Value::Int(3));
            assert_eq!(rows[0].values[2], spg_storage::Value::Int(31));
        }
        other => panic!("{other:?}"),
    }
    let r = e
        .execute(
            "MERGE INTO rv USING s ON rv.a = s.id \
             WHEN MATCHED AND rv.a = 3 THEN UPDATE SET b = 77 \
             RETURNING rv.a, rv.*",
        )
        .unwrap();
    match r {
        QueryResult::Rows { columns, rows } => {
            assert_eq!(columns[0].name, "a");
            assert_eq!(columns[1].name, "a");
            assert_eq!(columns[2].name, "b");
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0].values[0], spg_storage::Value::Int(3));
            assert_eq!(rows[0].values[1], spg_storage::Value::Int(3));
            assert_eq!(rows[0].values[2], spg_storage::Value::Int(77));
        }
        other => panic!("{other:?}"),
    }
}

/// P6 — a lower view with its OWN check option enforces even when the
/// written view has none: MERGE and plain UPDATE alike.
#[test]
fn inner_view_own_option_enforced_without_written_option() {
    let mut e = Engine::new();
    setup(&mut e);
    e.execute("CREATE VIEW inner_v AS SELECT id, v FROM t WHERE v < 1000 WITH CHECK OPTION")
        .unwrap();
    e.execute("CREATE VIEW outer_v AS SELECT id, v FROM inner_v")
        .unwrap();
    errs(
        &mut e,
        "MERGE INTO outer_v USING s ON outer_v.id = s.id WHEN MATCHED THEN UPDATE SET v = 5000",
        "new row violates check option for view \"inner_v\" DETAIL: Failing row contains (1, 5000).",
    );
    errs(
        &mut e,
        "UPDATE outer_v SET v = 6000 WHERE id = 2",
        "new row violates check option for view \"inner_v\" DETAIL: Failing row contains (2, 6000).",
    );
    errs(
        &mut e,
        "INSERT INTO outer_v VALUES (9, 7000)",
        "new row violates check option for view \"inner_v\" DETAIL: Failing row contains (9, 7000).",
    );
    assert_eq!(
        pairs(&mut e, "SELECT id, v FROM t ORDER BY id"),
        vec![(1, 10), (2, 20)]
    );
}
