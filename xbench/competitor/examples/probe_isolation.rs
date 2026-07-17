//! Phase-E isolation-level semantics inventory. PG18 anchors:
//! - READ COMMITTED: a statement sees rows committed BEFORE the
//!   statement (tx1 re-SELECT sees tx2's commit).
//! - REPEATABLE READ: tx keeps its first-snapshot view.
//! - SERIALIZABLE: RR reads + SSI write-conflict abort.

fn probe(level: &str) {
    use spg_engine::{Engine, IMPLICIT_TX, QueryResult};
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (x INT NOT NULL)").unwrap();
    e.execute("INSERT INTO t VALUES (1)").unwrap();
    let tx1 = e.alloc_tx_id();
    e.execute_in("BEGIN", tx1).unwrap();
    if !level.is_empty()
        && let Err(err) = e.execute_in(&format!("SET TRANSACTION ISOLATION LEVEL {level}"), tx1)
    {
        println!("{level:>16}: SET failed: {err:?}");
        return;
    }
    // Establish tx1's snapshot with a first read.
    let count = |e: &mut Engine, tx| -> String {
        match e.execute_in("SELECT count(*) FROM t", tx) {
            Ok(QueryResult::Rows { rows, .. }) => format!("{:?}", rows[0].values[0]),
            other => format!("{other:?}"),
        }
    };
    let before = count(&mut e, tx1);
    // Concurrent autocommit insert (committed immediately).
    e.execute_in("INSERT INTO t VALUES (2)", IMPLICIT_TX)
        .unwrap();
    let after = count(&mut e, tx1);
    // Write-write: tx1 updates a row someone else updated after tx1's
    // snapshot — SER should abort at some point (here or commit).
    let upd = e.execute_in("UPDATE t SET x = 10 WHERE x = 1", tx1);
    let commit = e.execute_in("COMMIT", tx1);
    println!(
        "{:>16}: first-read={before} after-concurrent-commit={after} update={} commit={}",
        if level.is_empty() { "(default)" } else { level },
        match upd {
            Ok(_) => "ok".to_string(),
            Err(e) => format!("ERR {e:?}"),
        },
        match commit {
            Ok(_) => "ok".to_string(),
            Err(e) => format!("ERR {e:?}"),
        },
    );
}

fn main() {
    probe("");
    probe("READ COMMITTED");
    probe("REPEATABLE READ");
    probe("SERIALIZABLE");
}
