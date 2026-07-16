//! v7.39 (read01 round 150) — referencing a data-modifying CTE that has
//! no RETURNING clause errors `WITH query "d" does not have a RETURNING
//! clause` (PG: 0A000, parse_relation.c addRangeTableEntryForCTE), and
//! because PG rejects at parse analysis, NO side effect lands — the CTE
//! body must not run. An unreferenced no-RETURNING body stays legal and
//! still executes. Locked byte-identical against PG 18.4 (10-case live
//! matrix). SPG previously materialised the alias as an empty temp table:
//! the outer query silently saw zero rows AND the modifying body ran.

use spg_engine::{Engine, QueryResult};

fn errs(e: &mut Engine, sql: &str, want: &str) {
    let m = match e.execute(sql) {
        Err(x) => format!("{x}"),
        Ok(r) => panic!("expected error for {sql}, got {r:?}"),
    };
    assert!(m.contains(want), "{m}");
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
    e.execute("CREATE TABLE t150(id int)").unwrap();
    e.execute("INSERT INTO t150 VALUES (1),(2)").unwrap();
    e.execute("CREATE TABLE s150(id int)").unwrap();
}

const WANT: &str = "does not have a RETURNING clause";

/// P1 — outer FROM reference errors and the DELETE must NOT have run.
#[test]
fn outer_ref_errors_without_side_effect() {
    let mut e = Engine::new();
    setup(&mut e);
    errs(
        &mut e,
        "WITH d AS (DELETE FROM t150) SELECT * FROM d",
        "WITH query \"d\" does not have a RETURNING clause",
    );
    assert_eq!(count(&mut e, "SELECT count(*) FROM t150"), 2);
}

/// P2 — an unreferenced no-RETURNING body is legal and executes.
#[test]
fn unreferenced_body_still_runs() {
    let mut e = Engine::new();
    setup(&mut e);
    assert_eq!(
        count(&mut e, "WITH d AS (DELETE FROM t150 WHERE id = 1) SELECT 1"),
        1
    );
    assert_eq!(count(&mut e, "SELECT count(*) FROM t150"), 1);
}

/// P3/P4/P9 — sibling-CTE and subquery references error too.
#[test]
fn sibling_and_subquery_refs_error() {
    let mut e = Engine::new();
    setup(&mut e);
    errs(
        &mut e,
        "WITH d AS (DELETE FROM t150), x AS (SELECT * FROM d) SELECT 1",
        WANT,
    );
    errs(
        &mut e,
        "WITH d AS (DELETE FROM t150) SELECT (SELECT count(*) FROM d)",
        WANT,
    );
    errs(
        &mut e,
        "WITH d AS (DELETE FROM t150), \
              i AS (INSERT INTO s150 SELECT id FROM d RETURNING id) \
         SELECT * FROM i",
        WANT,
    );
    assert_eq!(count(&mut e, "SELECT count(*) FROM t150"), 2);
}

/// P5/P6/P7 — INSERT / UPDATE bodies and JOIN-position references.
#[test]
fn insert_update_bodies_and_join_ref_error() {
    let mut e = Engine::new();
    setup(&mut e);
    errs(
        &mut e,
        "WITH i AS (INSERT INTO t150 VALUES (9)) SELECT * FROM i",
        "WITH query \"i\" does not have a RETURNING clause",
    );
    errs(
        &mut e,
        "WITH u AS (UPDATE t150 SET id = id + 10) SELECT t.id FROM t150 t, u",
        "WITH query \"u\" does not have a RETURNING clause",
    );
    errs(
        &mut e,
        "WITH d AS (DELETE FROM t150) SELECT * FROM s150 JOIN d ON d.id = s150.id",
        WANT,
    );
    assert_eq!(count(&mut e, "SELECT count(*) FROM t150"), 2);
}

/// P8 — the outer statement being a DML makes no difference; P10 — with
/// RETURNING the reference is fine (control).
#[test]
fn outer_dml_ref_errors_and_returning_control_works() {
    let mut e = Engine::new();
    setup(&mut e);
    errs(
        &mut e,
        "WITH d AS (DELETE FROM t150) INSERT INTO s150 SELECT id FROM d",
        WANT,
    );
    assert_eq!(count(&mut e, "SELECT count(*) FROM t150"), 2);
    assert_eq!(
        count(
            &mut e,
            "WITH d AS (DELETE FROM t150 WHERE id = 2 RETURNING id) SELECT * FROM d",
        ),
        2
    );
    assert_eq!(count(&mut e, "SELECT count(*) FROM t150"), 1);
}
