//! v7.39 (read01 round 146, PG17 parse_merge.c) — `WHEN NOT MATCHED BY
//! SOURCE` (target rows no source row matches; UPDATE / DELETE / DO NOTHING
//! only) and its `BY TARGET` synonym. Locked byte-identical against PG 18.4:
//! INSERT under BY SOURCE is a syntax error, a source-column reference in a
//! BY SOURCE clause is PG's "invalid reference" error, and the three-branch
//! MATCHED / BY SOURCE / BY TARGET combo produces PG's exact result.
//!
//! Also pins the storage-position fix this round surfaced: MERGE used the
//! VISIBLE-SNAPSHOT ordinal as a storage position when applying updates /
//! deletes, so once dead row versions preceded a target row (any prior MVCC
//! update or delete on the table) MERGE mutated the WRONG rows.

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
    e.execute("CREATE TABLE mt(id int, v int)").unwrap();
    e.execute("CREATE TABLE ms(id int, v int)").unwrap();
    e.execute("INSERT INTO mt VALUES (1,10),(2,20),(4,40)")
        .unwrap();
    e.execute("INSERT INTO ms VALUES (1,100),(3,300)").unwrap();
}

#[test]
fn by_source_conditional_update() {
    let mut e = Engine::new();
    setup(&mut e);
    // Unmatched-by-source rows are {2,4}; the AND keeps only v > 25 → row 4.
    assert_eq!(
        affected(
            &mut e,
            "MERGE INTO mt USING ms ON mt.id = ms.id WHEN NOT MATCHED BY SOURCE AND mt.v > 25 THEN UPDATE SET v = -1"
        ),
        1
    );
    assert_eq!(
        pairs(&mut e, "SELECT id, v FROM mt ORDER BY id"),
        vec![(1, 10), (2, 20), (4, -1)]
    );
}

#[test]
fn three_branch_combo_matches_pg() {
    let mut e = Engine::new();
    setup(&mut e);
    // MATCHED updates id=1; BY SOURCE deletes {2,4}; BY TARGET inserts id=3.
    assert_eq!(
        affected(
            &mut e,
            "MERGE INTO mt USING ms ON mt.id = ms.id WHEN MATCHED THEN UPDATE SET v = ms.v WHEN NOT MATCHED BY SOURCE THEN DELETE WHEN NOT MATCHED BY TARGET THEN INSERT VALUES (ms.id, ms.v)"
        ),
        4
    );
    assert_eq!(
        pairs(&mut e, "SELECT id, v FROM mt ORDER BY id"),
        vec![(1, 100), (3, 300)]
    );
    // A second BY SOURCE pass finds nothing (every target row now matches).
    assert_eq!(
        affected(
            &mut e,
            "MERGE INTO mt USING ms ON mt.id = ms.id WHEN NOT MATCHED BY SOURCE THEN DELETE"
        ),
        0
    );
}

#[test]
fn insert_under_by_source_is_syntax_error() {
    let mut e = Engine::new();
    setup(&mut e);
    let m = match e.execute(
        "MERGE INTO mt USING ms ON mt.id = ms.id WHEN NOT MATCHED BY SOURCE THEN INSERT VALUES (0,0)",
    ) {
        Err(x) => format!("{x}"),
        Ok(_) => panic!("expected error"),
    };
    assert!(m.contains("syntax error at or near \"INSERT\""), "{m}");
}

#[test]
fn source_column_ref_in_by_source_rejected() {
    let mut e = Engine::new();
    setup(&mut e);
    let m = match e.execute(
        "MERGE INTO mt USING ms ON mt.id = ms.id WHEN NOT MATCHED BY SOURCE THEN UPDATE SET v = ms.v",
    ) {
        Err(x) => format!("{x}"),
        Ok(_) => panic!("expected error"),
    };
    assert!(
        m.contains("invalid reference to FROM-clause entry for table \"ms\""),
        "{m}"
    );
}

#[test]
fn positions_survive_prior_mvcc_versions() {
    // Under default MVCC, an UPDATE tombstones the old version and appends
    // the new one — snapshot ordinals then diverge from storage positions.
    // MERGE previously deleted/updated by ordinal, hitting the wrong rows.
    let mut e = Engine::new();
    setup(&mut e);
    // Create a dead version ahead of rows 2 and 4.
    e.execute("UPDATE mt SET v = 11 WHERE id = 1").unwrap();
    // Now delete the unmatched-by-source rows {2,4}.
    assert_eq!(
        affected(
            &mut e,
            "MERGE INTO mt USING ms ON mt.id = ms.id WHEN NOT MATCHED BY SOURCE THEN DELETE"
        ),
        2
    );
    assert_eq!(
        pairs(&mut e, "SELECT id, v FROM mt ORDER BY id"),
        vec![(1, 11)]
    );
    // And a MATCHED update after further churn lands on the right row.
    e.execute("INSERT INTO mt VALUES (5,50)").unwrap();
    e.execute("UPDATE mt SET v = 12 WHERE id = 1").unwrap();
    assert_eq!(
        affected(
            &mut e,
            "MERGE INTO mt USING ms ON mt.id = ms.id WHEN MATCHED THEN UPDATE SET v = ms.v"
        ),
        1
    );
    assert_eq!(
        pairs(&mut e, "SELECT id, v FROM mt ORDER BY id"),
        vec![(1, 100), (5, 50)]
    );
}

#[test]
fn by_source_returning() {
    let mut e = Engine::new();
    setup(&mut e);
    match e
        .execute("MERGE INTO mt USING ms ON mt.id = ms.id WHEN NOT MATCHED BY SOURCE AND mt.v > 25 THEN DELETE RETURNING merge_action(), mt.id")
        .unwrap()
    {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows.len(), 1);
            match (&rows[0].values[0], &rows[0].values[1]) {
                (spg_storage::Value::Text(a), spg_storage::Value::Int(id)) => {
                    assert_eq!(a.as_ref(), "DELETE");
                    assert_eq!(*id, 4);
                }
                other => panic!("{other:?}"),
            }
        }
        other => panic!("{other:?}"),
    }
}
