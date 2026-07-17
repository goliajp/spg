//! E4 round 3: RC unique-key collision under rebase (suspected P0 —
//! E3 added the unique pre-check only to the RR/SER merge), plus
//! ON CONFLICT and trigger interaction sanity.

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

fn rc_unique_collision() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE u (x INT UNIQUE)").unwrap();
    let tx = e.alloc_tx_id();
    e.execute_in("BEGIN", tx).unwrap();
    e.execute_in("INSERT INTO u VALUES (7)", tx).unwrap();
    e.execute_in("INSERT INTO u VALUES (7)", IMPLICIT_TX)
        .unwrap();
    // Next tx statement rebases — does the replayed insert duplicate 7?
    println!(
        "uq1 tx view (PG: blocks then 23505 at stmt/commit): {}",
        q(&mut e, "SELECT x FROM u", tx)
    );
    let commit = e.execute_in("COMMIT", tx);
    println!("uq2 commit: {commit:?}");
    println!(
        "uq3 final (must be ONE row): {}",
        q(&mut e, "SELECT x FROM u", IMPLICIT_TX)
    );
}

fn on_conflict_x_rebase() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE u (x INT PRIMARY KEY, n INT NOT NULL)")
        .unwrap();
    e.execute("INSERT INTO u VALUES (1, 0)").unwrap();
    let tx = e.alloc_tx_id();
    e.execute_in("BEGIN", tx).unwrap();
    e.execute_in(
        "INSERT INTO u VALUES (1, 0) ON CONFLICT (x) DO UPDATE SET n = u.n + 10",
        tx,
    )
    .unwrap();
    e.execute_in(
        "INSERT INTO u VALUES (1, 0) ON CONFLICT (x) DO UPDATE SET n = u.n + 100",
        IMPLICIT_TX,
    )
    .unwrap();
    println!("oc1 tx view: {}", q(&mut e, "SELECT x, n FROM u", tx));
    let commit = e.execute_in("COMMIT", tx);
    println!("oc2 commit: {commit:?}");
    println!(
        "oc3 final (one row): {}",
        q(&mut e, "SELECT x, n FROM u", IMPLICIT_TX)
    );
}

fn main() {
    rc_unique_collision();
    on_conflict_x_rebase();
}
