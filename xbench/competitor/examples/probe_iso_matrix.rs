//! E4 scenario probe: concurrent UPDATE-UPDATE under RC. Suspected E2
//! atomicity hole: an UPDATE's write-set is (tombstone old, insert new)
//! with NO pairing — a rebase that skips the conflicting tombstone
//! still replays the insert, DUPLICATING the row.

fn main() {
    use spg_engine::{Engine, IMPLICIT_TX, QueryResult};
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (x INT NOT NULL)").unwrap();
    e.execute("INSERT INTO t VALUES (1)").unwrap();
    let tx = e.alloc_tx_id();
    e.execute_in("BEGIN", tx).unwrap();
    e.execute_in("UPDATE t SET x = 10 WHERE x = 1", tx).unwrap();
    e.execute_in("UPDATE t SET x = 20 WHERE x = 1", IMPLICIT_TX)
        .unwrap();
    let dump = |e: &mut Engine, tag: &str, tx| {
        if let Ok(QueryResult::Rows { rows, .. }) = e.execute_in("SELECT x FROM t ORDER BY x", tx) {
            let xs: Vec<String> = rows.iter().map(|r| format!("{:?}", r.values[0])).collect();
            println!("{tag}: [{}]", xs.join(", "));
        }
    };
    dump(&mut e, "tx1 after rebase (PG: one row)", tx);
    e.execute_in("COMMIT", tx).unwrap();
    dump(&mut e, "after commit (PG: one row x=10)", IMPLICIT_TX);
}
