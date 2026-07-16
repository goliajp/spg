//! v7.39 (read01 round 149) — a DML target never resolves to a CTE.
//! PG's parse analysis resolves INSERT / UPDATE / DELETE / MERGE targets
//! only against real relations, so `WITH c AS (…) DELETE FROM c` errors
//! `relation "c" does not exist` — for the outer statement and for a
//! sibling data-modifying CTE body alike (6-case live matrix vs PG 18.4).
//! SPG's temp-table CTE machinery used to resolve the target to the
//! just-installed temp: the write landed in the temp and vanished when
//! the statement ended (silent-wrong; pre-existing for INSERT / UPDATE /
//! DELETE since the T4.4 writable-CTE work, new surface for MERGE).

use spg_engine::{Engine, QueryResult};

fn errs(e: &mut Engine, sql: &str, want: &str) {
    let m = match e.execute(sql) {
        Err(x) => format!("{x}"),
        Ok(r) => panic!("expected error for {sql}, got {r:?}"),
    };
    assert!(m.contains(want), "{m}");
}

fn setup(e: &mut Engine) {
    e.execute("CREATE TABLE rt(id int)").unwrap();
    e.execute("INSERT INTO rt VALUES (1)").unwrap();
}

#[test]
fn outer_dml_target_is_cte_errors() {
    let mut e = Engine::new();
    setup(&mut e);
    errs(
        &mut e,
        "WITH c AS (SELECT 1 AS id) MERGE INTO c USING rt s ON c.id = s.id \
         WHEN MATCHED THEN DO NOTHING",
        "relation \"c\" does not exist",
    );
    errs(
        &mut e,
        "WITH c AS (SELECT 1 AS id) DELETE FROM c",
        "relation \"c\" does not exist",
    );
    errs(
        &mut e,
        "WITH c AS (SELECT 1 AS id) UPDATE c SET id = 2",
        "relation \"c\" does not exist",
    );
    errs(
        &mut e,
        "WITH c AS (SELECT 1 AS id) INSERT INTO c VALUES (2)",
        "relation \"c\" does not exist",
    );
    // The guard must not have leaked writes anywhere.
    match e.execute("SELECT count(*) FROM rt").unwrap() {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(format!("{:?}", rows[0].values[0]), "BigInt(1)");
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn cte_body_dml_target_is_sibling_cte_errors() {
    let mut e = Engine::new();
    setup(&mut e);
    errs(
        &mut e,
        "WITH a AS (SELECT 1 AS id), d AS (DELETE FROM a RETURNING id) SELECT * FROM d",
        "relation \"a\" does not exist",
    );
    errs(
        &mut e,
        "WITH a AS (SELECT 1 AS id), \
              m AS (MERGE INTO a USING rt s ON a.id = s.id \
                    WHEN MATCHED THEN DO NOTHING RETURNING id) \
         SELECT * FROM m",
        "relation \"a\" does not exist",
    );
    // Positive control — a real-table target next to CTEs keeps working.
    match e
        .execute("WITH c AS (SELECT 5 AS id) INSERT INTO rt SELECT id FROM c")
        .unwrap()
    {
        QueryResult::CommandOk { affected, .. } => assert_eq!(affected, 1),
        other => panic!("{other:?}"),
    }
}
