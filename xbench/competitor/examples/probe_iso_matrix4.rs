//! E4 round 4: trigger×rebase (does the trigger's embedded write into a
//! second table survive a concurrent commit + rebase?), RETURNING×rebase,
//! and RR savepoint×merge.

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
        Err(err) => format!("ERR({err})"),
    }
}

fn trigger_x_rebase() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE main (id INT NOT NULL)").unwrap();
    e.execute("CREATE TABLE log (id INT NOT NULL)").unwrap();
    e.execute(
        "CREATE FUNCTION audit() RETURNS TRIGGER LANGUAGE plpgsql AS $$
BEGIN
  INSERT INTO log VALUES (NEW.id);
  RETURN NEW;
END;
$$",
    )
    .unwrap();
    e.execute("CREATE TRIGGER tg AFTER INSERT ON main FOR EACH ROW EXECUTE FUNCTION audit()")
        .unwrap();
    let tx = e.alloc_tx_id();
    e.execute_in("BEGIN", tx).unwrap();
    e.execute_in("INSERT INTO main VALUES (1)", tx).unwrap();
    // Concurrent autocommit insert — its trigger writes log(2).
    e.execute_in("INSERT INTO main VALUES (2)", IMPLICIT_TX)
        .unwrap();
    // Next tx statement rebases; then commit.
    println!(
        "tr1 tx main: {}",
        q(&mut e, "SELECT id FROM main ORDER BY id", tx)
    );
    println!(
        "tr2 tx log:  {}",
        q(&mut e, "SELECT id FROM log ORDER BY id", tx)
    );
    let commit = e.execute_in("COMMIT", tx);
    println!("tr3 commit: {commit:?}");
    println!(
        "tr4 final main (want 1|2): {}",
        q(&mut e, "SELECT id FROM main ORDER BY id", IMPLICIT_TX)
    );
    println!(
        "tr5 final log  (want 1|2): {}",
        q(&mut e, "SELECT id FROM log ORDER BY id", IMPLICIT_TX)
    );
}

fn returning_x_rebase() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT NOT NULL, n INT NOT NULL)")
        .unwrap();
    e.execute("INSERT INTO t VALUES (1, 10), (2, 20)").unwrap();
    let tx = e.alloc_tx_id();
    e.execute_in("BEGIN", tx).unwrap();
    println!(
        "rt1 tx UPDATE..RETURNING (want 11): {}",
        q(
            &mut e,
            "UPDATE t SET n = n + 1 WHERE id = 1 RETURNING n",
            tx
        )
    );
    e.execute_in("UPDATE t SET n = n + 100 WHERE id = 2", IMPLICIT_TX)
        .unwrap();
    println!(
        "rt2 tx view: {}",
        q(&mut e, "SELECT id, n FROM t ORDER BY id", tx)
    );
    let commit = e.execute_in("COMMIT", tx);
    println!("rt3 commit: {commit:?}");
    println!(
        "rt4 final (want 1,11 | 2,120): {}",
        q(&mut e, "SELECT id, n FROM t ORDER BY id", IMPLICIT_TX)
    );
}

fn rr_savepoint_x_merge() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT NOT NULL, n INT NOT NULL)")
        .unwrap();
    e.execute("INSERT INTO t VALUES (1, 10), (2, 20), (3, 30)")
        .unwrap();
    let tx = e.alloc_tx_id();
    e.execute_in("BEGIN", tx).unwrap();
    e.execute_in("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ", tx)
        .unwrap();
    e.execute_in("UPDATE t SET n = 11 WHERE id = 1", tx)
        .unwrap();
    e.execute_in("SAVEPOINT s1", tx).unwrap();
    e.execute_in("UPDATE t SET n = 22 WHERE id = 2", tx)
        .unwrap();
    e.execute_in("ROLLBACK TO SAVEPOINT s1", tx).unwrap();
    // Concurrent write to a third, untouched row.
    e.execute_in("UPDATE t SET n = 33 WHERE id = 3", IMPLICIT_TX)
        .unwrap();
    let commit = e.execute_in("COMMIT", tx);
    println!("sp1 commit (want Ok): {commit:?}");
    println!(
        "sp2 final (want 1,11 | 2,20 | 3,33): {}",
        q(&mut e, "SELECT id, n FROM t ORDER BY id", IMPLICIT_TX)
    );
}

fn main() {
    trigger_x_rebase();
    returning_x_rebase();
    rr_savepoint_x_merge();
}
