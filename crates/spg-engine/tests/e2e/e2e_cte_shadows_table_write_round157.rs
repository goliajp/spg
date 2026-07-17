//! v7.39 (read01 round 157) — CTE-shadows-table on the WRITE path
//! (round 156 covered reads). The write-path machinery installs temps on
//! the live catalog, so a shadowing CTE gets a renamed temp and every
//! read reference rewrites to it. PG scoping (r157 live probes P1-P8):
//! DML outers read the CTE (P1-P5); a DML TARGET always resolves to the
//! REAL table even beside a same-named CTE, while the same statement's
//! read references still see the CTE (P6/P7); a shadowing MODIFYING body
//! writes the real table and the outer reads the CTE's RETURNING (P8).

use spg_engine::{Engine, QueryResult};

fn col(e: &mut Engine, sql: &str) -> Vec<i64> {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => rows
            .iter()
            .map(|r| match r.values[0] {
                spg_storage::Value::BigInt(n) => n,
                spg_storage::Value::Int(n) => i64::from(n),
                ref other => panic!("{other:?}"),
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
    e.execute("CREATE TABLE wt157(x int)").unwrap();
    e.execute("INSERT INTO wt157 VALUES (100),(200)").unwrap();
    e.execute("CREATE TABLE wo157(id int)").unwrap();
}

/// Probe-revealed pre-existing bug (no shadow involved): the dispatch-
/// level uncorrelated-subquery pass ran BEFORE the CTE temps installed,
/// so `WITH c AS (…) UPDATE … SET x = (SELECT … FROM c)` errored
/// "relation c does not exist" (and with a same-named real table it
/// silently read THAT — the CardinalityViolation that exposed this).
/// The pass now runs inside the CTE machinery, temps first.
#[test]
fn update_delete_subqueries_read_plain_cte() {
    let mut e = Engine::new();
    setup(&mut e);
    e.execute("INSERT INTO wo157 VALUES (1)").unwrap();
    assert_eq!(
        affected(
            &mut e,
            "WITH c AS (SELECT 5 AS x) \
             UPDATE wo157 SET id = (SELECT x FROM c) WHERE id = 1",
        ),
        1
    );
    assert_eq!(
        affected(
            &mut e,
            "WITH c AS (SELECT 5 AS x) DELETE FROM wo157 WHERE id IN (SELECT x FROM c)",
        ),
        1
    );
    assert_eq!(col(&mut e, "SELECT count(*) FROM wo157"), vec![0]);
}

/// P1-P3 — INSERT / UPDATE / DELETE outers read the shadowing CTE.
#[test]
fn dml_outers_read_the_shadow() {
    let mut e = Engine::new();
    setup(&mut e);
    assert_eq!(
        affected(
            &mut e,
            "WITH wt157 AS (SELECT 1 AS x) INSERT INTO wo157 SELECT x FROM wt157",
        ),
        1
    );
    assert_eq!(col(&mut e, "SELECT id FROM wo157"), vec![1]);
    assert_eq!(
        affected(
            &mut e,
            "WITH wt157 AS (SELECT 5 AS x) \
             UPDATE wo157 SET id = (SELECT x FROM wt157) WHERE id = 1",
        ),
        1
    );
    assert_eq!(
        affected(
            &mut e,
            "WITH wt157 AS (SELECT 5 AS x) \
             DELETE FROM wo157 WHERE id IN (SELECT x FROM wt157)",
        ),
        1
    );
    assert_eq!(col(&mut e, "SELECT count(*) FROM wo157"), vec![0]);
    // The real table is untouched throughout.
    assert_eq!(col(&mut e, "SELECT count(*) FROM wt157"), vec![2]);
}

/// P4 — a modifying sibling beside the shadow; the outer reads it.
#[test]
fn modifying_sibling_beside_shadow() {
    let mut e = Engine::new();
    setup(&mut e);
    e.execute("INSERT INTO wo157 VALUES (7),(8)").unwrap();
    assert_eq!(
        col(
            &mut e,
            "WITH wt157 AS (SELECT 7 AS x), \
                  d AS (DELETE FROM wo157 WHERE id IN (SELECT x FROM wt157) RETURNING id) \
             SELECT * FROM d",
        ),
        vec![7]
    );
    assert_eq!(col(&mut e, "SELECT id FROM wo157"), vec![8]);
}

/// P5 — MERGE USING the shadow.
#[test]
fn merge_using_the_shadow() {
    let mut e = Engine::new();
    setup(&mut e);
    e.execute("INSERT INTO wo157 VALUES (8)").unwrap();
    assert_eq!(
        affected(
            &mut e,
            "WITH wt157 AS (SELECT 8 AS x) \
             MERGE INTO wo157 USING wt157 s ON wo157.id = s.x \
             WHEN MATCHED THEN UPDATE SET id = 88",
        ),
        1
    );
    assert_eq!(col(&mut e, "SELECT id FROM wo157"), vec![88]);
}

/// P6/P7 — the DML target IS the real table while the reads see the CTE.
#[test]
fn target_is_real_table_reads_are_cte() {
    let mut e = Engine::new();
    setup(&mut e);
    assert_eq!(
        affected(
            &mut e,
            "WITH wt157 AS (SELECT 9 AS x) INSERT INTO wt157 SELECT x FROM wt157",
        ),
        1
    );
    assert_eq!(
        col(&mut e, "SELECT x FROM wt157 ORDER BY x"),
        vec![9, 100, 200]
    );
    assert_eq!(
        affected(
            &mut e,
            "WITH wt157 AS (SELECT 9 AS x) \
             UPDATE wt157 SET x = 10 WHERE x IN (SELECT x FROM wt157)",
        ),
        1
    );
    assert_eq!(
        col(&mut e, "SELECT x FROM wt157 ORDER BY x"),
        vec![10, 100, 200]
    );
}

/// P8 — the shadowing CTE's own body is modifying and writes the REAL
/// table; the outer reads the CTE's RETURNING.
#[test]
fn shadowing_modifying_body_writes_real_table() {
    let mut e = Engine::new();
    setup(&mut e);
    assert_eq!(
        col(
            &mut e,
            "WITH wt157 AS (INSERT INTO wt157 VALUES (300) RETURNING x) SELECT * FROM wt157",
        ),
        vec![300]
    );
    assert_eq!(
        col(&mut e, "SELECT x FROM wt157 ORDER BY x"),
        vec![100, 200, 300]
    );
}
