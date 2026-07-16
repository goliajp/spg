//! v7.39 (read01 round 149) — WITH + MERGE, both directions:
//! `WITH <ctes> MERGE INTO …` (PG 15; each CTE materialises first and its
//! alias resolves as a source relation, including data-modifying CTEs) and
//! MERGE as a data-modifying CTE body (`WITH m AS (MERGE … RETURNING …)
//! SELECT …`, PG 17). `WITH RECURSIVE` + MERGE is rejected with PG's exact
//! message. Locked byte-identical against PG 18.4 (9-case live matrix).
//! SPG previously reported a parse error for every one of these shapes.

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

fn count(e: &mut Engine, sql: &str) -> i64 {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => match rows[0].values[0] {
            spg_storage::Value::BigInt(n) => n,
            spg_storage::Value::Int(n) => i64::from(n),
            ref other => panic!("{other:?}"),
        },
        other => panic!("{other:?}"),
    }
}

fn setup(e: &mut Engine) {
    e.execute("CREATE TABLE tgt(id int, v int)").unwrap();
    e.execute("INSERT INTO tgt VALUES (1,10),(2,20)").unwrap();
    e.execute("CREATE TABLE src(id int, v int)").unwrap();
    e.execute("INSERT INTO src VALUES (1,100),(3,300)").unwrap();
}

/// P1 — basic `WITH s AS (SELECT …) MERGE`: the CTE alias is the source.
/// PG: MERGE 2, tgt = {1:100, 2:20, 3:300}.
#[test]
fn with_select_cte_feeds_merge() {
    let mut e = Engine::new();
    setup(&mut e);
    assert_eq!(
        affected(
            &mut e,
            "WITH s AS (SELECT * FROM src) \
             MERGE INTO tgt t USING s ON t.id = s.id \
             WHEN MATCHED THEN UPDATE SET v = s.v \
             WHEN NOT MATCHED THEN INSERT VALUES (s.id, s.v)"
        ),
        2
    );
    assert_eq!(
        pairs(&mut e, "SELECT id, v FROM tgt ORDER BY id"),
        vec![(1, 100), (2, 20), (3, 300)]
    );
}

/// P2 + P3 — chained CTEs (second references first) and a CTE referenced
/// inside a USING subquery, run in the probe's exact sequence (after P1's
/// merge, so tgt = {1:100, 2:20, 3:300}). PG: MERGE 1 each;
/// tgt = {1:101, 2:20, 3:600} afterwards.
#[test]
fn chained_ctes_and_cte_in_using_subquery() {
    let mut e = Engine::new();
    setup(&mut e);
    affected(
        &mut e,
        "WITH s AS (SELECT * FROM src) \
         MERGE INTO tgt t USING s ON t.id = s.id \
         WHEN MATCHED THEN UPDATE SET v = s.v \
         WHEN NOT MATCHED THEN INSERT VALUES (s.id, s.v)",
    );
    assert_eq!(
        affected(
            &mut e,
            "WITH a AS (SELECT id, v*2 AS v FROM src), \
                  b AS (SELECT * FROM a WHERE id > 1) \
             MERGE INTO tgt t USING b ON t.id = b.id \
             WHEN MATCHED THEN UPDATE SET v = b.v"
        ),
        1
    );
    assert_eq!(
        affected(
            &mut e,
            "WITH s AS (SELECT * FROM src) \
             MERGE INTO tgt t USING (SELECT id, v+1 AS v FROM s) x ON t.id = x.id \
             WHEN MATCHED AND t.id = 1 THEN UPDATE SET v = x.v"
        ),
        1
    );
    assert_eq!(
        pairs(&mut e, "SELECT id, v FROM tgt ORDER BY id"),
        vec![(1, 101), (2, 20), (3, 600)]
    );
}

/// P4 — `WITH RECURSIVE` heading a MERGE is rejected with PG's message.
#[test]
fn with_recursive_merge_rejected() {
    let mut e = Engine::new();
    setup(&mut e);
    let m = match e.execute(
        "WITH RECURSIVE r(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM r WHERE n < 3) \
         MERGE INTO tgt t USING r ON t.id = r.n \
         WHEN MATCHED THEN UPDATE SET v = 0",
    ) {
        Err(x) => format!("{x}"),
        Ok(r) => panic!("expected error, got {r:?}"),
    };
    assert!(
        m.contains("WITH RECURSIVE is not supported for MERGE statement"),
        "{m}"
    );
}

/// P5 — a data-modifying CTE feeds the MERGE: the DELETE runs (its
/// RETURNING rows are the source) and the merge inserts them.
/// PG: MERGE 2, other emptied.
#[test]
fn modifying_cte_feeds_merge() {
    let mut e = Engine::new();
    setup(&mut e);
    e.execute("CREATE TABLE other(id int)").unwrap();
    e.execute("INSERT INTO other VALUES (5),(6)").unwrap();
    assert_eq!(
        affected(
            &mut e,
            "WITH d AS (DELETE FROM other RETURNING id) \
             MERGE INTO tgt t USING d ON t.id = d.id \
             WHEN NOT MATCHED THEN INSERT VALUES (d.id, -1)"
        ),
        2
    );
    assert_eq!(
        pairs(&mut e, "SELECT id, v FROM tgt ORDER BY id"),
        vec![(1, 10), (2, 20), (5, -1), (6, -1)]
    );
    assert_eq!(count(&mut e, "SELECT count(*) FROM other"), 0);
}

/// P6 + P7 — MERGE as a CTE body (PG 17): with RETURNING the alias holds
/// the projected rows; without RETURNING it materialises empty but the
/// merge still runs.
#[test]
fn merge_as_cte_body() {
    let mut e = Engine::new();
    setup(&mut e);
    // With RETURNING — rows visible through the alias, writes land.
    let r = e
        .execute(
            "WITH m AS (\
               MERGE INTO tgt t USING src s ON t.id = s.id \
               WHEN MATCHED THEN UPDATE SET v = t.v + 1 \
               RETURNING merge_action() AS act, t.id, t.v) \
             SELECT act, id, v FROM m ORDER BY id",
        )
        .unwrap();
    match r {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows.len(), 1);
            assert_eq!(format!("{:?}", rows[0].values[0]), "Text(\"UPDATE\")");
            assert_eq!(rows[0].values[1], spg_storage::Value::Int(1));
            assert_eq!(rows[0].values[2], spg_storage::Value::Int(11));
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(
        pairs(&mut e, "SELECT id, v FROM tgt ORDER BY id"),
        vec![(1, 11), (2, 20)]
    );
    // Without RETURNING — the merge still runs (PG allows this).
    assert_eq!(
        count(
            &mut e,
            "WITH m AS (\
               MERGE INTO tgt t USING src s ON t.id = s.id \
               WHEN MATCHED THEN UPDATE SET v = t.v + 1) \
             SELECT 1",
        ),
        1
    );
    assert_eq!(
        pairs(&mut e, "SELECT id, v FROM tgt ORDER BY id"),
        vec![(1, 12), (2, 20)]
    );
}

/// P8 + P9 — WITH + MERGE + RETURNING, and a WITH-narrowed source under
/// WHEN NOT MATCHED BY SOURCE (PG: MERGE 5 — 1 update + 4 deletes).
#[test]
fn with_merge_returning_and_by_source() {
    let mut e = Engine::new();
    setup(&mut e);
    let r = e
        .execute(
            "WITH s AS (SELECT * FROM src) \
             MERGE INTO tgt t USING s ON t.id = s.id \
             WHEN MATCHED THEN UPDATE SET v = s.v \
             RETURNING merge_action(), t.id, t.v",
        )
        .unwrap();
    match r {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0].values[1], spg_storage::Value::Int(1));
            assert_eq!(rows[0].values[2], spg_storage::Value::Int(100));
        }
        other => panic!("{other:?}"),
    }
    e.execute("INSERT INTO tgt VALUES (3,30),(5,-1),(6,-1)")
        .unwrap();
    assert_eq!(
        affected(
            &mut e,
            "WITH s AS (SELECT * FROM src WHERE id = 1) \
             MERGE INTO tgt t USING s ON t.id = s.id \
             WHEN MATCHED THEN UPDATE SET v = 999 \
             WHEN NOT MATCHED BY SOURCE THEN DELETE"
        ),
        5
    );
    assert_eq!(
        pairs(&mut e, "SELECT id, v FROM tgt ORDER BY id"),
        vec![(1, 999)]
    );
}
