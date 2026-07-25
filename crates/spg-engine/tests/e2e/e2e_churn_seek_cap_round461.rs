//! read01 round 461 — a churned table must keep using its index.
//!
//! The mutation paths' index seek is capped so that an index walk never
//! costs more than the scan it replaces. The seek returns one locator per
//! row VERSION, though, and a churned table's index still carries the dead
//! ones — so the count being compared against the cap was inflated by
//! exactly the rows the caller was about to discard.
//!
//! Measured before the fix, deleting and reinserting the same 1000 rows on a
//! 50k-row table with autovacuum off: by cycle 20 the seek was refused and
//! every DELETE became a 71000-row scan, 0.23 ms -> 4.48 ms. The seek it
//! refused would have walked 22000 candidates — three times cheaper than the
//! scan it fell back to.

use spg_engine::{Engine, QueryResult};

fn stat(e: &mut Engine, col: &str) -> i64 {
    match e
        .execute(&format!(
            "SELECT {col} FROM pg_stat_user_tables WHERE relname='wb'"
        ))
        .unwrap()
    {
        QueryResult::Rows { rows, .. } => spg_engine::eval::value_to_text(&rows[0].values[0])
            .parse()
            .unwrap_or(-1),
        other => panic!("{other:?}"),
    }
}

fn insert_sql(base: i64, rows: i64) -> String {
    let mut s = String::from("INSERT INTO wb VALUES ");
    for k in 0..rows {
        if k > 0 {
            s.push(',');
        }
        s.push_str(&format!("({},{})", base + k, (base + k) % 7));
    }
    s
}

#[test]
fn round461_churned_table_still_seeks_instead_of_scanning() {
    let mut e = Engine::new();
    // Autovacuum off is the server's exposure: it runs the reclaim in a
    // background worker, which a tight delete-reinsert loop outruns.
    e.set_autovacuum(false);
    e.execute("CREATE TABLE wb(id INT PRIMARY KEY, g INT)").unwrap();
    for chunk in 0..5i64 {
        e.execute(&insert_sql(chunk * 1000, 1000)).unwrap();
    }

    let seg = 2000i64;
    let del = format!("DELETE FROM wb WHERE id >= {seg} AND id < {}", seg + 1000);
    let ins = insert_sql(seg, 1000);

    // Churn until dead rows are well past a quarter of the table — the point
    // the old cap started refusing the seek.
    for _ in 0..12 {
        e.execute(&del).unwrap();
        e.execute(&ins).unwrap();
    }
    assert!(
        stat(&mut e, "n_dead_tup") > 5000 / 4,
        "the fixture must actually push dead rows past a quarter of the table"
    );

    let before = stat(&mut e, "seq_tup_read");
    e.execute(&del).unwrap();
    assert_eq!(
        stat(&mut e, "seq_tup_read") - before,
        0,
        "a churned table's range DELETE must still reach the index, not scan"
    );
}

#[test]
fn round461_a_wide_range_still_declines_the_seek() {
    // The other half of the contract: the cap still has to refuse a seek
    // that would return most of the table, or it stops being a cap.
    let mut e = Engine::new();
    e.execute("CREATE TABLE wb(id INT PRIMARY KEY, g INT)").unwrap();
    for chunk in 0..5i64 {
        e.execute(&insert_sql(chunk * 1000, 1000)).unwrap();
    }
    let before = stat(&mut e, "seq_tup_read");
    // Every row matches.
    e.execute("DELETE FROM wb WHERE id >= 0 AND id < 5000").unwrap();
    assert!(
        stat(&mut e, "seq_tup_read") - before > 0,
        "a whole-table range must still take the scan"
    );
}
