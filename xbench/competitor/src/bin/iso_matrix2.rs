//! v7.39 (round 497) — the second cross-session differential sweep.
//!
//! Rounds 494-496 found three ways a COMMIT deleted another session's
//! committed data, all in the same shadow-catalog install and all invisible
//! to single-connection tests. That says the surface was never differenced,
//! not that those three were the only ones — so this covers what the first
//! sweep did not: sequences, savepoints, TRUNCATE, and same-name DDL races.
//!
//! Sequences are the sharpest question here. PG's `nextval` is explicitly
//! NON-transactional: a rollback does not give the number back, because
//! concurrent sessions must never receive the same value. SPG keeps
//! sequence state in the catalog, and a transaction works on a catalog
//! CLONE — so the model predicts a rollback restores the counter.
//!
//!   ISO_URL=postgres://…  cargo run --release --bin iso_matrix2

use sqlx::{AnyConnection, Connection as _, Executor as _, Row as _};

async fn cell(c: &mut AnyConnection, sql: &str) -> String {
    match c.fetch_all(sql).await {
        Ok(rows) => rows
            .first()
            .and_then(|r| {
                r.try_get::<i32, _>(0)
                    .map(|v| v.to_string())
                    .or_else(|_| r.try_get::<i64, _>(0).map(|v| v.to_string()))
                    .ok()
            })
            .unwrap_or_else(|| "<none>".into()),
        Err(e) => {
            let s = e.to_string();
            format!("ERR({})", s.split(':').next_back().unwrap_or("?").trim())
        }
    }
}

async fn run(c: &mut AnyConnection, sql: &str) -> bool {
    c.execute(sql).await.is_ok()
}

async fn fresh(url: &str) -> Result<AnyConnection, sqlx::Error> {
    let mut c = AnyConnection::connect(url).await?;
    // Several scenarios deliberately put one session in another's way, and
    // PG blocks on the lock — correct, and a deadlock for a sequential
    // harness. Bound the wait so the cell reads "ERR(timeout)" instead of
    // hanging. SPG may not know these knobs; failing to set them is fine.
    let _ = c.execute("SET lock_timeout = '2s'").await;
    let _ = c.execute("SET statement_timeout = '5s'").await;
    Ok(c)
}

#[allow(clippy::too_many_lines)] // one self-contained scenario per block
#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    sqlx::any::install_default_drivers();
    let url = std::env::var("ISO_URL").map_err(|_| "set ISO_URL")?;
    println!("| # | scenario | expected (PG18) | result |");
    println!("|---|----------|-----------------|--------|");

    // S1 — a rolled-back nextval must NOT be handed out again.
    {
        let mut a = fresh(&url).await?;
        run(&mut a, "DROP SEQUENCE IF EXISTS q1").await;
        run(&mut a, "CREATE SEQUENCE q1").await;
        let first = cell(&mut a, "SELECT nextval('q1')").await;
        run(&mut a, "BEGIN").await;
        let inside = cell(&mut a, "SELECT nextval('q1')").await;
        run(&mut a, "ROLLBACK").await;
        let after = cell(&mut a, "SELECT nextval('q1')").await;
        println!("| S1 | nextval 1, BEGIN, nextval, ROLLBACK, nextval | 1/2/3 | {first}/{inside}/{after} |");
    }

    // S2 — two sessions must never receive the same sequence value, even
    // when one of them is inside a transaction that later rolls back.
    {
        let (mut a, mut b) = (fresh(&url).await?, fresh(&url).await?);
        run(&mut a, "DROP SEQUENCE IF EXISTS q2").await;
        run(&mut a, "CREATE SEQUENCE q2").await;
        run(&mut a, "BEGIN").await;
        let a1 = cell(&mut a, "SELECT nextval('q2')").await;
        let b1 = cell(&mut b, "SELECT nextval('q2')").await;
        run(&mut a, "ROLLBACK").await;
        let b2 = cell(&mut b, "SELECT nextval('q2')").await;
        let dup = a1 == b1 || a1 == b2 || b1 == b2;
        println!("| S2 | A-in-tx, B, A rolls back, B again | 1/2/3 no dup | {a1}/{b1}/{b2} dup={dup} |");
    }

    // S3 — a savepoint rollback must not take another session's commit
    // with it.
    {
        let (mut a, mut b) = (fresh(&url).await?, fresh(&url).await?);
        run(&mut a, "DROP TABLE IF EXISTS s3").await;
        run(&mut a, "CREATE TABLE s3 (id INT PRIMARY KEY, v INT)").await;
        run(&mut a, "INSERT INTO s3 VALUES (1, 10), (2, 20)").await;
        run(&mut a, "BEGIN ISOLATION LEVEL REPEATABLE READ").await;
        run(&mut a, "UPDATE s3 SET v = 111 WHERE id = 2").await;
        run(&mut a, "SAVEPOINT sp").await;
        run(&mut a, "UPDATE s3 SET v = 222 WHERE id = 2").await;
        run(&mut b, "UPDATE s3 SET v = 99 WHERE id = 1").await;
        run(&mut a, "ROLLBACK TO SAVEPOINT sp").await;
        run(&mut a, "COMMIT").await;
        let mut c = fresh(&url).await?;
        let r1 = cell(&mut c, "SELECT v FROM s3 WHERE id = 1").await;
        let r2 = cell(&mut c, "SELECT v FROM s3 WHERE id = 2").await;
        println!("| S3 | RR + SAVEPOINT rollback; B wrote row 1 | 99/111 | {r1}/{r2} |");
    }

    // S4 — TRUNCATE inside a transaction, another session writing another
    // table.
    {
        let (mut a, mut b) = (fresh(&url).await?, fresh(&url).await?);
        run(&mut a, "DROP TABLE IF EXISTS s4a").await;
        run(&mut a, "DROP TABLE IF EXISTS s4b").await;
        run(&mut a, "CREATE TABLE s4a (id INT PRIMARY KEY)").await;
        run(&mut a, "CREATE TABLE s4b (id INT PRIMARY KEY, v INT)").await;
        run(&mut a, "INSERT INTO s4a VALUES (1), (2)").await;
        run(&mut a, "INSERT INTO s4b VALUES (1, 10)").await;
        run(&mut a, "BEGIN ISOLATION LEVEL REPEATABLE READ").await;
        run(&mut a, "TRUNCATE s4a").await;
        run(&mut b, "UPDATE s4b SET v = 99 WHERE id = 1").await;
        run(&mut a, "COMMIT").await;
        let mut c = fresh(&url).await?;
        let left = cell(&mut c, "SELECT count(*)::int FROM s4a").await;
        let other = cell(&mut c, "SELECT v FROM s4b WHERE id = 1").await;
        println!("| S4 | RR + TRUNCATE s4a; B wrote s4b | 0/99 | {left}/{other} |");
    }

    // S5 — a transaction that inserts, and another that inserts the same
    // primary key: exactly one may survive.
    {
        let (mut a, mut b) = (fresh(&url).await?, fresh(&url).await?);
        run(&mut a, "DROP TABLE IF EXISTS s5").await;
        run(&mut a, "CREATE TABLE s5 (id INT PRIMARY KEY, who INT)").await;
        run(&mut a, "BEGIN ISOLATION LEVEL REPEATABLE READ").await;
        let a_ins = run(&mut a, "INSERT INTO s5 VALUES (1, 1)").await;
        let b_ins = run(&mut b, "INSERT INTO s5 VALUES (1, 2)").await;
        let a_commit = run(&mut a, "COMMIT").await;
        let mut c = fresh(&url).await?;
        let n = cell(&mut c, "SELECT count(*)::int FROM s5").await;
        println!("| S5 | both INSERT pk 1 (A in RR) | 1 row | {n} rows, A ins={a_ins} B ins={b_ins} A commit={a_commit} |");
    }

    // S6 — DROP in one session while another writes the SAME table.
    {
        let (mut a, mut b) = (fresh(&url).await?, fresh(&url).await?);
        run(&mut a, "DROP TABLE IF EXISTS s6").await;
        run(&mut a, "CREATE TABLE s6 (id INT PRIMARY KEY, v INT)").await;
        run(&mut a, "INSERT INTO s6 VALUES (1, 10)").await;
        run(&mut a, "BEGIN ISOLATION LEVEL REPEATABLE READ").await;
        run(&mut a, "INSERT INTO s6 VALUES (2, 20)").await;
        let b_drop = run(&mut b, "DROP TABLE s6").await;
        let a_commit = run(&mut a, "COMMIT").await;
        let mut c = fresh(&url).await?;
        let exists = cell(&mut c, "SELECT count(*)::int FROM s6").await;
        println!("| S6 | A writes s6, B DROPs it, A commits | table gone | {exists} (B drop={b_drop}, A commit={a_commit}) |");
    }

    // S7 — currval is per-session state, not shared.
    {
        let (mut a, mut b) = (fresh(&url).await?, fresh(&url).await?);
        run(&mut a, "DROP SEQUENCE IF EXISTS q7").await;
        run(&mut a, "CREATE SEQUENCE q7").await;
        let _ = cell(&mut a, "SELECT nextval('q7')").await;
        let b_curr = cell(&mut b, "SELECT currval('q7')").await;
        let a_curr = cell(&mut a, "SELECT currval('q7')").await;
        println!("| S7 | A nextval, B currval / A currval | ERR / 1 | {b_curr} / {a_curr} |");
    }
    Ok(())
}
