//! read01 round 432 (MySQL differential) — `DELETE … [ORDER BY …] [LIMIT n]`.
//!
//! MySQL's batched-cleanup idiom: delete the N oldest rows, loop until the
//! table is drained. SPG had the UPDATE half of this clause since round 413
//! but not the DELETE half, so `DELETE … ORDER BY … LIMIT` was a parse
//! error — and there is no rewrite a client can apply that keeps the same
//! meaning, since dropping the LIMIT deletes the whole matching set.
//!
//! Both statements now share ONE parse routine and ONE payload type
//! (`DmlOrderLimit`), so they cannot drift on, say, whether `LIMIT 0` is
//! legal.
//!
//! Measured on MariaDB 11 over the same five rows
//! `(1,5,'a') (2,3,'B') (3,9,'c') (4,3,'d') (5,1,'e')`:
//!   ORDER BY v DESC LIMIT 2        → 2,4,5 remain
//!   LIMIT 2 (no ORDER BY)          → 3,4,5 remain (storage order)
//!   WHERE v>=3 ORDER BY v LIMIT 2  → 1,3,5 remain
//!   LIMIT 0                        → nothing deleted
//!   LIMIT 99                       → all deleted
//!   ORDER BY v ASC, id DESC LIMIT 2→ 1,2,3 remain
//!   ORDER BY v (no LIMIT)          → all deleted
//!   on ('B','a','C'): ORDER BY s LIMIT 2 → only id 3 remains (CI order)
//!   LIMIT 1,2 (offset form)        → ERROR 1064, rejected
//!   ORDER BY v DESC LIMIT 1 RETURNING id → 3
//!
//! Every expectation is copied from that run.

use spg_engine::{Engine, QueryResult};

fn mysql() -> Engine {
    let mut e = Engine::new();
    e.execute("SET sql_mode='STRICT_TRANS_TABLES'").unwrap();
    e.execute("CREATE TABLE t(id INT PRIMARY KEY, v INT, s VARCHAR(10))")
        .unwrap();
    fill(&mut e);
    e
}

fn fill(e: &mut Engine) {
    e.execute("INSERT INTO t VALUES (1,5,'a'),(2,3,'B'),(3,9,'c'),(4,3,'d'),(5,1,'e')")
        .unwrap();
}

fn ids(e: &mut Engine) -> String {
    match e.execute("SELECT id FROM t ORDER BY id").unwrap() {
        QueryResult::Rows { rows, .. } => rows
            .iter()
            .map(|r| spg_engine::eval::value_to_text(&r.values[0]))
            .collect::<Vec<_>>()
            .join(","),
        other => panic!("{other:?}"),
    }
}

/// Run `sql`, then assert the surviving ids.
fn survives(sql: &str, want: &str) {
    let mut e = mysql();
    e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err}"));
    assert_eq!(ids(&mut e), want, "{sql}");
}

#[test]
fn round432_order_by_desc_limit_deletes_the_largest() {
    survives("DELETE FROM t ORDER BY v DESC LIMIT 2", "2,4,5");
}

#[test]
fn round432_limit_without_order_by_takes_storage_order() {
    survives("DELETE FROM t LIMIT 2", "3,4,5");
}

#[test]
fn round432_where_narrows_before_the_order_and_limit() {
    // Candidates are v>=3 (ids 1,2,3,4); the two smallest v among them are
    // ids 2 and 4 (both v=3).
    survives("DELETE FROM t WHERE v >= 3 ORDER BY v ASC LIMIT 2", "1,3,5");
}

#[test]
fn round432_limit_zero_deletes_nothing_and_limit_past_the_end_deletes_all() {
    survives("DELETE FROM t ORDER BY v LIMIT 0", "1,2,3,4,5");
    survives("DELETE FROM t ORDER BY v LIMIT 99", "");
}

#[test]
fn round432_order_by_uses_the_session_collation() {
    // 'B','a','C' discriminates: MariaDB's case-insensitive default sorts
    // a < B < C, so ids 2 and 1 go and 3 survives (measured). A byte-order
    // sort would put 'B'(0x42) < 'C'(0x43) < 'a'(0x61) and leave id 2.
    let mut e = Engine::new();
    e.execute("SET sql_mode='STRICT_TRANS_TABLES'").unwrap();
    e.execute("CREATE TABLE c(id INT PRIMARY KEY, s VARCHAR(10))")
        .unwrap();
    e.execute("INSERT INTO c VALUES (1,'B'),(2,'a'),(3,'C')")
        .unwrap();
    e.execute("DELETE FROM c ORDER BY s LIMIT 2").unwrap();
    match e.execute("SELECT id FROM c ORDER BY id").unwrap() {
        QueryResult::Rows { rows, .. } => {
            let got: Vec<String> = rows
                .iter()
                .map(|r| spg_engine::eval::value_to_text(&r.values[0]))
                .collect();
            assert_eq!(got, ["3"]);
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn round432_multi_column_order_by_breaks_ties() {
    // v ascending: id5 (v=1), then the v=3 tie between id2 and id4, broken
    // by id DESC → id4. So ids 5 and 4 go.
    survives("DELETE FROM t ORDER BY v ASC, id DESC LIMIT 2", "1,2,3");
}

#[test]
fn round432_order_by_without_limit_deletes_every_matching_row() {
    survives("DELETE FROM t ORDER BY v", "");
}

#[test]
fn round432_offset_form_is_rejected() {
    // MariaDB: ERROR 1064 near '2'. A DELETE LIMIT takes a row count only.
    let mut e = mysql();
    let err = e
        .execute("DELETE FROM t ORDER BY v LIMIT 1,2")
        .expect_err("offset form must be rejected");
    assert!(
        format!("{err}").contains("offset"),
        "expected an offset-rejection message, got {err}"
    );
    assert_eq!(
        ids(&mut e),
        "1,2,3,4,5",
        "the rejected statement must not delete"
    );
}

#[test]
fn round432_returning_trails_the_limit() {
    let mut e = mysql();
    match e
        .execute("DELETE FROM t ORDER BY v DESC LIMIT 1 RETURNING id")
        .unwrap()
    {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows.len(), 1);
            assert_eq!(spg_engine::eval::value_to_text(&rows[0].values[0]), "3");
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(ids(&mut e), "1,2,4,5");
}

#[test]
fn round432_affected_count_is_the_narrowed_set() {
    let mut e = mysql();
    match e.execute("DELETE FROM t ORDER BY v DESC LIMIT 2").unwrap() {
        QueryResult::CommandOk { affected, .. } => assert_eq!(affected, 2),
        other => panic!("{other:?}"),
    }
}

#[test]
fn round432_pg_dialect_still_rejects_the_clause() {
    // PG has no ORDER BY / LIMIT on DELETE; a PG session must keep erroring.
    let mut e = Engine::new();
    e.execute("CREATE TABLE t(id INT PRIMARY KEY, v INT)")
        .unwrap();
    e.execute("INSERT INTO t VALUES (1,1),(2,2)").unwrap();
    let err = e
        .execute("DELETE FROM t ORDER BY v LIMIT 1")
        .expect_err("PG must reject DELETE … ORDER BY");
    assert!(format!("{err}").contains("ORDER"), "{err}");
}
