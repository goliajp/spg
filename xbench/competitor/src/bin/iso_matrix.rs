//! v7.39 (round 495) — the cross-session isolation matrix, differentially.
//!
//! Round 494 found a read-only REPEATABLE READ commit deleting another
//! session's committed writes, and the reason it survived is structural:
//! COMMIT installs the transaction's shadow catalog wholesale unless a
//! rebase or merge path claims it first, and each of those paths has its
//! own gate. Round 494 closed one uncovered gate (no writes at all). The
//! others are `rebase_poisoned` — set by DDL, MERGE, SET, or any
//! statement the classifier does not recognise — and the write-to-a-
//! different-table case.
//!
//! Single-connection tests cannot pose any of these. This runs each
//! scenario against whatever `ISO_URL` points at, so PG18 supplies the
//! expected column.
//!
//!   ISO_URL=postgres://…  cargo run --release --bin iso_matrix

use sqlx::{AnyConnection, Connection as _, Executor as _, Row as _};

async fn cell(c: &mut AnyConnection, sql: &str) -> String {
    match c.fetch_all(sql).await {
        Ok(rows) => rows
            .first()
            .and_then(|r| r.try_get::<i32, _>(0).ok())
            .map_or_else(|| "<none>".into(), |v| v.to_string()),
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
    AnyConnection::connect(url).await
}

async fn reset(c: &mut AnyConnection) -> Result<(), sqlx::Error> {
    c.execute("DROP TABLE IF EXISTS m1").await?;
    c.execute("DROP TABLE IF EXISTS m2").await?;
    c.execute("CREATE TABLE m1 (id INT PRIMARY KEY, v INT)").await?;
    c.execute("CREATE TABLE m2 (id INT PRIMARY KEY, v INT)").await?;
    c.execute("INSERT INTO m1 VALUES (1, 10), (2, 20)").await?;
    c.execute("INSERT INTO m2 VALUES (1, 10)").await?;
    Ok(())
}

// One scenario per block, each self-contained; splitting them into
// helpers would hide the sequence that makes the matrix readable.
#[allow(clippy::too_many_lines)]
#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    sqlx::any::install_default_drivers();
    let url = std::env::var("ISO_URL").map_err(|_| "set ISO_URL")?;
    println!("| # | scenario | result |");
    println!("|---|----------|--------|");

    // M1 — READ COMMITTED sees another session's commit mid-transaction.
    {
        let (mut a, mut b) = (fresh(&url).await?, fresh(&url).await?);
        reset(&mut a).await?;
        run(&mut a, "BEGIN").await;
        let _ = cell(&mut a, "SELECT v FROM m1 WHERE id = 1").await;
        run(&mut b, "UPDATE m1 SET v = 99 WHERE id = 1").await;
        let mid = cell(&mut a, "SELECT v FROM m1 WHERE id = 1").await;
        run(&mut a, "COMMIT").await;
        println!("| M1 | RC reads again mid-tx (PG: 99) | {mid} |");
    }

    // M2 — REPEATABLE READ does not.
    {
        let (mut a, mut b) = (fresh(&url).await?, fresh(&url).await?);
        reset(&mut a).await?;
        run(&mut a, "BEGIN ISOLATION LEVEL REPEATABLE READ").await;
        let _ = cell(&mut a, "SELECT v FROM m1 WHERE id = 1").await;
        run(&mut b, "UPDATE m1 SET v = 99 WHERE id = 1").await;
        let mid = cell(&mut a, "SELECT v FROM m1 WHERE id = 1").await;
        run(&mut a, "COMMIT").await;
        println!("| M2 | RR reads again mid-tx (PG: 10) | {mid} |");
    }

    // M3 — an RR transaction that WROTE a different row: does its commit
    // keep the other session's committed write to a different row?
    {
        let (mut a, mut b) = (fresh(&url).await?, fresh(&url).await?);
        reset(&mut a).await?;
        run(&mut a, "BEGIN ISOLATION LEVEL REPEATABLE READ").await;
        run(&mut a, "UPDATE m1 SET v = 111 WHERE id = 2").await;
        run(&mut b, "UPDATE m1 SET v = 99 WHERE id = 1").await;
        run(&mut a, "COMMIT").await;
        let mut c = fresh(&url).await?;
        let row1 = cell(&mut c, "SELECT v FROM m1 WHERE id = 1").await;
        let row2 = cell(&mut c, "SELECT v FROM m1 WHERE id = 2").await;
        println!("| M3 | RR wrote row 2; B wrote row 1 (PG: 99 / 111) | {row1} / {row2} |");
    }

    // M4 — an RR transaction that wrote a DIFFERENT TABLE.
    {
        let (mut a, mut b) = (fresh(&url).await?, fresh(&url).await?);
        reset(&mut a).await?;
        run(&mut a, "BEGIN ISOLATION LEVEL REPEATABLE READ").await;
        run(&mut a, "UPDATE m2 SET v = 111 WHERE id = 1").await;
        run(&mut b, "UPDATE m1 SET v = 99 WHERE id = 1").await;
        run(&mut a, "COMMIT").await;
        let mut c = fresh(&url).await?;
        let m1v = cell(&mut c, "SELECT v FROM m1 WHERE id = 1").await;
        let m2v = cell(&mut c, "SELECT v FROM m2 WHERE id = 1").await;
        println!("| M4 | RR wrote m2; B wrote m1 (PG: 99 / 111) | {m1v} / {m2v} |");
    }

    // M5 — an RR transaction that also ran a statement the classifier does
    // not recognise as DML (here SET), which poisons the rebase.
    {
        let (mut a, mut b) = (fresh(&url).await?, fresh(&url).await?);
        reset(&mut a).await?;
        run(&mut a, "BEGIN ISOLATION LEVEL REPEATABLE READ").await;
        run(&mut a, "SET application_name = 'iso_matrix'").await;
        run(&mut a, "UPDATE m1 SET v = 111 WHERE id = 2").await;
        run(&mut b, "UPDATE m1 SET v = 99 WHERE id = 1").await;
        run(&mut a, "COMMIT").await;
        let mut c = fresh(&url).await?;
        let row1 = cell(&mut c, "SELECT v FROM m1 WHERE id = 1").await;
        let row2 = cell(&mut c, "SELECT v FROM m1 WHERE id = 2").await;
        println!("| M5 | RR + SET, wrote row 2; B wrote row 1 (PG: 99 / 111) | {row1} / {row2} |");
    }

    // M6 — an RR transaction that ran DDL, which also poisons.
    {
        let (mut a, mut b) = (fresh(&url).await?, fresh(&url).await?);
        reset(&mut a).await?;
        run(&mut a, "BEGIN ISOLATION LEVEL REPEATABLE READ").await;
        run(&mut a, "CREATE TABLE m3 (a INT)").await;
        run(&mut b, "UPDATE m1 SET v = 99 WHERE id = 1").await;
        run(&mut a, "COMMIT").await;
        let mut c = fresh(&url).await?;
        let row1 = cell(&mut c, "SELECT v FROM m1 WHERE id = 1").await;
        let made = cell(&mut c, "SELECT count(*)::int FROM m3").await;
        let _ = c.execute("DROP TABLE IF EXISTS m3").await;
        println!("| M6 | RR + DDL; B wrote row 1 (PG: 99 / 0) | {row1} / {made} |");
    }

    // M7 — a read-only SERIALIZABLE transaction, round 494's sibling.
    {
        let (mut a, mut b) = (fresh(&url).await?, fresh(&url).await?);
        reset(&mut a).await?;
        run(&mut a, "BEGIN ISOLATION LEVEL SERIALIZABLE").await;
        let _ = cell(&mut a, "SELECT v FROM m1 WHERE id = 1").await;
        run(&mut b, "UPDATE m1 SET v = 99 WHERE id = 1").await;
        run(&mut a, "COMMIT").await;
        let mut c = fresh(&url).await?;
        println!(
            "| M7 | read-only SERIALIZABLE commits (PG: 99) | {} |",
            cell(&mut c, "SELECT v FROM m1 WHERE id = 1").await
        );
    }

    // M8 — both sessions update the SAME row: first-committer-wins.
    {
        let (mut a, mut b) = (fresh(&url).await?, fresh(&url).await?);
        reset(&mut a).await?;
        run(&mut a, "BEGIN ISOLATION LEVEL REPEATABLE READ").await;
        let _ = cell(&mut a, "SELECT v FROM m1 WHERE id = 1").await;
        run(&mut b, "UPDATE m1 SET v = 99 WHERE id = 1").await;
        let a_upd = run(&mut a, "UPDATE m1 SET v = 111 WHERE id = 1").await;
        let a_commit = run(&mut a, "COMMIT").await;
        let mut c = fresh(&url).await?;
        println!(
            "| M8 | both update row 1; A's UPDATE ok={a_upd} commit ok={a_commit} (PG: false/false, value 99) | {} |",
            cell(&mut c, "SELECT v FROM m1 WHERE id = 1").await
        );
    }
    Ok(())
}
