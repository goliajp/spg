//! read01 round 490 (P0-25) — the index seek stops handing back dead versions.
//!
//! A BTree index holds one locator per row VERSION. On a churned table the
//! dead ones are still in there, so a 1000-row range came back with 61 000
//! candidates after 60 delete-and-reinsert cycles with the background
//! vacuum off. Every caller dropped them again — the mutation paths and
//! the SELECT range path all test `is_row_visible` and `continue` — but
//! only after collecting them into a `Vec`, sorting, and walking.
//!
//! Now the walk applies that same test, so the cap counts rows a caller
//! will look at and the candidate list stays proportional to the LIVE
//! matches. Measured on the churn probe: candidates 61 000 -> 1 000 at
//! cycle 60, and the DELETE 0.868 ms -> 0.301.
//!
//! What these pin is the part that could go wrong: dropping a version the
//! statement was supposed to see. `idx_scan` / `seq_tup_read` are the
//! engine's own witnesses for "did this go through the index or the
//! table", so the counts assert the seek still engages rather than
//! silently falling back to a scan that happens to give the right answer.

use spg_engine::{Engine, QueryResult};

fn one(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => rows
            .iter()
            .map(|r| {
                r.values
                    .iter()
                    .map(spg_engine::eval::value_to_text)
                    .collect::<Vec<_>>()
                    .join("|")
            })
            .collect::<Vec<_>>()
            .join(";"),
        other => panic!("{sql} -> {other:?}"),
    }
}

fn stat(e: &mut Engine, col: &str) -> i64 {
    one(e, &format!(
        "SELECT {col} FROM pg_stat_user_tables WHERE relname='t'"
    ))
    .parse()
    .unwrap_or(-1)
}

fn churned(cycles: usize) -> Engine {
    let mut e = Engine::new();
    // Autovacuum off reproduces the server's exposure: it runs reclamation
    // in a background worker, and a tight delete/reinsert loop outruns it.
    e.set_autovacuum(false);
    e.execute("CREATE TABLE t (id INT PRIMARY KEY, g INT)").unwrap();
    let mut vals = String::from("INSERT INTO t VALUES ");
    for i in 0..5000 {
        if i > 0 {
            vals.push(',');
        }
        vals.push_str(&format!("({i},{})", i % 10));
    }
    e.execute(&vals).unwrap();
    for _ in 0..cycles {
        e.execute("DELETE FROM t WHERE id >= 1000 AND id < 1200").unwrap();
        let mut re = String::from("INSERT INTO t VALUES ");
        for i in 1000..1200 {
            if i > 1000 {
                re.push(',');
            }
            re.push_str(&format!("({i},{})", i % 10));
        }
        e.execute(&re).unwrap();
    }
    e
}

#[test]
fn round490_churn_does_not_change_what_a_range_delete_removes() {
    for cycles in [0usize, 5, 30] {
        let mut e = churned(cycles);
        assert_eq!(one(&mut e, "SELECT count(*) FROM t"), "5000", "cycles={cycles}");
        assert_eq!(
            one(&mut e, "SELECT count(*) FROM t WHERE id >= 1000 AND id < 1200"),
            "200",
            "cycles={cycles}"
        );
        e.execute("DELETE FROM t WHERE id >= 1000 AND id < 1200").unwrap();
        assert_eq!(one(&mut e, "SELECT count(*) FROM t"), "4800", "cycles={cycles}");
        assert_eq!(
            one(&mut e, "SELECT count(*) FROM t WHERE id >= 1000 AND id < 1200"),
            "0",
            "cycles={cycles}"
        );
    }
}

#[test]
fn round490_churn_does_not_change_what_a_range_update_touches() {
    for cycles in [0usize, 5, 30] {
        let mut e = churned(cycles);
        let n = e.execute("UPDATE t SET g = 77 WHERE id >= 1000 AND id < 1200");
        assert!(n.is_ok(), "cycles={cycles}: {n:?}");
        assert_eq!(
            one(&mut e, "SELECT count(*) FROM t WHERE g = 77"),
            "200",
            "cycles={cycles}"
        );
        // Exactly the intended band moved; the rows either side did not.
        assert_eq!(
            one(&mut e, "SELECT count(*) FROM t WHERE g = 77 AND (id < 1000 OR id >= 1200)"),
            "0",
            "cycles={cycles}"
        );
    }
}

#[test]
fn round490_equality_seek_still_finds_the_live_version() {
    // The equality seek got the same filter. A row deleted and reinserted
    // 30 times has 30 dead versions under its key and exactly one live one.
    let mut e = churned(30);
    assert_eq!(one(&mut e, "SELECT g FROM t WHERE id = 1100"), "0");
    e.execute("UPDATE t SET g = 5 WHERE id = 1100").unwrap();
    assert_eq!(one(&mut e, "SELECT g FROM t WHERE id = 1100"), "5");
    e.execute("DELETE FROM t WHERE id = 1100").unwrap();
    assert_eq!(one(&mut e, "SELECT count(*) FROM t WHERE id = 1100"), "0");
    assert_eq!(one(&mut e, "SELECT count(*) FROM t"), "4999");
}

#[test]
fn round490_range_mutation_still_uses_the_index() {
    // The answer above would also be right if the seek had been refused and
    // the statement fell back to a table scan. These are the engine's own
    // witnesses that it did not: a range mutation reports an index scan and
    // reads no sequential tuples, while a deliberately unindexed predicate
    // reports the opposite.
    let mut e = churned(30);
    let idx_before = stat(&mut e, "idx_scan");
    let seq_before = stat(&mut e, "seq_tup_read");
    e.execute("DELETE FROM t WHERE id >= 1000 AND id < 1200").unwrap();
    assert_eq!(stat(&mut e, "idx_scan") - idx_before, 1, "range DELETE seeks");
    assert_eq!(
        stat(&mut e, "seq_tup_read") - seq_before,
        0,
        "range DELETE does not scan"
    );

    let seq_mid = stat(&mut e, "seq_tup_read");
    e.execute("DELETE FROM t WHERE g = 999999").unwrap();
    assert!(
        stat(&mut e, "seq_tup_read") - seq_mid > 0,
        "unindexed DELETE does scan"
    );
}
