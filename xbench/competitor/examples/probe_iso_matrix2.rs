//! E4 matrix round 2: savepoint×rebase, multi-table, FK-across-tables,
//! delete-then-insert-same-key. Prints actuals for comparison with
//! hand-derived PG semantics.

use spg_engine::{Engine, IMPLICIT_TX, QueryResult, TxId};

fn q(e: &mut Engine, sql: &str, tx: TxId) -> String {
    match e.execute_in(sql, tx) {
        Ok(QueryResult::Rows { rows, .. }) => {
            let cells: Vec<String> = rows
                .iter()
                .map(|r| {
                    r.values
                        .iter()
                        .map(|v| format!("{v:?}"))
                        .collect::<Vec<_>>()
                        .join(",")
                })
                .collect();
            format!("[{}]", cells.join(" | "))
        }
        Ok(other) => format!("{other:?}"),
        Err(err) => format!("ERR {err:?}"),
    }
}

fn savepoint_x_rebase() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (x INT NOT NULL)").unwrap();
    let tx = e.alloc_tx_id();
    e.execute_in("BEGIN", tx).unwrap();
    e.execute_in("INSERT INTO t VALUES (1)", tx).unwrap();
    e.execute_in("SAVEPOINT s", tx).unwrap();
    e.execute_in("INSERT INTO t VALUES (2)", tx).unwrap();
    e.execute_in("INSERT INTO t VALUES (100)", IMPLICIT_TX).unwrap();
    println!("sp1 pre-rollback (PG [1,2,100]): {}", q(&mut e, "SELECT x FROM t ORDER BY x", tx));
    e.execute_in("ROLLBACK TO SAVEPOINT s", tx).unwrap();
    println!("sp2 post-rollback (PG [1,100]): {}", q(&mut e, "SELECT x FROM t ORDER BY x", tx));
    e.execute_in("COMMIT", tx).unwrap();
    println!("sp3 committed (PG [1,100]): {}", q(&mut e, "SELECT x FROM t ORDER BY x", IMPLICIT_TX));
}

fn multi_table() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE a (x INT NOT NULL)").unwrap();
    e.execute("CREATE TABLE b (y INT NOT NULL)").unwrap();
    let tx = e.alloc_tx_id();
    e.execute_in("BEGIN", tx).unwrap();
    e.execute_in("INSERT INTO a VALUES (1)", tx).unwrap();
    e.execute_in("INSERT INTO b VALUES (10)", tx).unwrap();
    e.execute_in("INSERT INTO a VALUES (2)", IMPLICIT_TX).unwrap();
    e.execute_in("INSERT INTO b VALUES (20)", IMPLICIT_TX).unwrap();
    println!(
        "mt1 a (PG [1,2]): {}  b (PG [10,20]): {}",
        q(&mut e, "SELECT x FROM a ORDER BY x", tx),
        q(&mut e, "SELECT y FROM b ORDER BY y", tx)
    );
    e.execute_in("COMMIT", tx).unwrap();
    println!(
        "mt2 committed a: {} b: {}",
        q(&mut e, "SELECT x FROM a ORDER BY x", IMPLICIT_TX),
        q(&mut e, "SELECT y FROM b ORDER BY y", IMPLICIT_TX)
    );
}

fn fk_orphan() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE p (id INT PRIMARY KEY)").unwrap();
    e.execute("CREATE TABLE c (pid INT REFERENCES p(id))").unwrap();
    e.execute("INSERT INTO p VALUES (1)").unwrap();
    let tx = e.alloc_tx_id();
    e.execute_in("BEGIN", tx).unwrap();
    e.execute_in("INSERT INTO c VALUES (1)", tx).unwrap();
    // Concurrent delete of the parent: PG blocks on the child's
    // FOR KEY SHARE lock, then fails with FK violation after commit.
    let del = q(&mut e, "DELETE FROM p WHERE id = 1", IMPLICIT_TX);
    println!("fk1 concurrent parent delete (PG: blocks/23503): {del}");
    let commit = e.execute_in("COMMIT", tx);
    println!("fk2 tx commit (PG: ok, delete failed): {commit:?}");
    println!(
        "fk3 orphan check — parent (PG [1]): {} child (PG [1]): {}",
        q(&mut e, "SELECT id FROM p", IMPLICIT_TX),
        q(&mut e, "SELECT pid FROM c", IMPLICIT_TX)
    );
}

fn delete_then_insert_same_key() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT PRIMARY KEY, v INT NOT NULL)").unwrap();
    e.execute("INSERT INTO t VALUES (1, 10)").unwrap();
    let tx = e.alloc_tx_id();
    e.execute_in("BEGIN", tx).unwrap();
    e.execute_in("DELETE FROM t WHERE id = 1", tx).unwrap();
    e.execute_in("INSERT INTO t VALUES (1, 11)", tx).unwrap();
    e.execute_in("DELETE FROM t WHERE id = 1", IMPLICIT_TX).unwrap();
    println!("di1 tx view (PG [1,11]): {}", q(&mut e, "SELECT id, v FROM t", tx));
    let commit = e.execute_in("COMMIT", tx);
    println!("di2 commit (PG ok): {commit:?}");
    println!("di3 final (PG [1,11]): {}", q(&mut e, "SELECT id, v FROM t", IMPLICIT_TX));
}

fn main() {
    savepoint_x_rebase();
    multi_table();
    fk_orphan();
    delete_then_insert_same_key();
}
