//! read01 round 455 — `pg_stat_user_tables` must count the tuples a DML
//! statement reads.
//!
//! PG's `seq_tup_read` covers every statement that walks a table, DML
//! included. SPG only reported from `scan_visible`, which the UPDATE and
//! DELETE executors do not use — they build their own candidate list — so a
//! mutation that scanned every row reported reading none. Any monitoring
//! built on that column was silently wrong for write workloads, and the
//! number a tool would use to spot a missing index was exactly the number
//! that stayed at zero.
//!
//! Measured before the fix: `DELETE` with an unindexed predicate over 50k
//! rows reported `seq_tup_read` 0.

use spg_engine::{Engine, QueryResult};

fn stat(e: &mut Engine, col: &str) -> i64 {
    let sql = format!("SELECT {col} FROM pg_stat_user_tables WHERE relname='t'");
    match e.execute(&sql).unwrap() {
        QueryResult::Rows { rows, .. } => spg_engine::eval::value_to_text(&rows[0].values[0])
            .parse()
            .unwrap_or(-1),
        other => panic!("{other:?}"),
    }
}

fn seeded(rows: i64) -> Engine {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t(id INT PRIMARY KEY, g INT)")
        .unwrap();
    let mut sql = String::from("INSERT INTO t VALUES ");
    for k in 0..rows {
        if k > 0 {
            sql.push(',');
        }
        sql.push_str(&format!("({k},{})", k % 7));
    }
    e.execute(&sql).unwrap();
    e
}

#[test]
fn round455_delete_without_an_index_counts_the_rows_it_read() {
    let mut e = seeded(500);
    let before = stat(&mut e, "seq_tup_read");
    // `g` is unindexed, so this walks the table.
    e.execute("DELETE FROM t WHERE g = 99").unwrap();
    let read = stat(&mut e, "seq_tup_read") - before;
    assert!(
        read >= 500,
        "a DELETE that scanned 500 rows must report reading them, got {read}"
    );
}

#[test]
fn round455_update_without_an_index_counts_the_rows_it_read() {
    let mut e = seeded(500);
    let before = stat(&mut e, "seq_tup_read");
    e.execute("UPDATE t SET g = g WHERE g = 99").unwrap();
    let read = stat(&mut e, "seq_tup_read") - before;
    assert!(
        read >= 500,
        "an UPDATE that scanned 500 rows must report reading them, got {read}"
    );
}

#[test]
fn round455_indexed_mutation_reports_no_sequential_read() {
    // The other half of the contract: a statement the index answers must not
    // inflate seq_tup_read, or the column stops distinguishing the two.
    let mut e = seeded(500);
    let before = stat(&mut e, "seq_tup_read");
    e.execute("DELETE FROM t WHERE id = 42").unwrap();
    assert_eq!(
        stat(&mut e, "seq_tup_read") - before,
        0,
        "an indexed DELETE must not report sequential reads"
    );
}
