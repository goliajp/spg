//! v7.39 (round 494) — cross-session snapshot isolation, over the wire.
//!
//! Round 493 tried to pin that a REPEATABLE READ transaction keeps its
//! view while another session commits over it, and the assertion failed —
//! identically with that round's change compiled out, so the question is
//! older than it. That test used the embedded engine's `set_current_session`,
//! which may or may not be the shape a customer meets, so this asks the
//! same question the way a customer would: two connections.
//!
//! Point it at either engine; the output is the differential.
//!
//!   ISO_URL=postgres://…  cargo run --release --bin iso_cross_session

use sqlx::{AnyConnection, Connection as _, Executor as _, Row as _};

async fn scalar(c: &mut AnyConnection, sql: &str) -> String {
    match c.fetch_all(sql).await {
        Ok(rows) => rows
            .first()
            .and_then(|r| r.try_get::<i32, _>(0).ok())
            .map_or_else(|| "<none>".into(), |v| v.to_string()),
        Err(e) => format!("ERR {e}"),
    }
}

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    sqlx::any::install_default_drivers();
    let url = std::env::var("ISO_URL").map_err(|_| "set ISO_URL")?;
    let mut a = AnyConnection::connect(&url).await?;
    let mut b = AnyConnection::connect(&url).await?;

    a.execute("DROP TABLE IF EXISTS iso").await?;
    a.execute("CREATE TABLE iso (id INT PRIMARY KEY, v INT)")
        .await?;
    a.execute("INSERT INTO iso VALUES (1, 10), (2, 20)").await?;

    println!("| step | reader sees |");
    println!("|------|------------:|");

    // Reader opens REPEATABLE READ and takes its snapshot with a read.
    a.execute("BEGIN ISOLATION LEVEL REPEATABLE READ").await?;
    println!(
        "| reader BEGIN + first read | {} |",
        scalar(&mut a, "SELECT v FROM iso WHERE id = 1").await
    );

    // Writer commits an UPDATE.
    b.execute("UPDATE iso SET v = 11 WHERE id = 1").await?;
    println!(
        "| after writer UPDATE commits | {} |",
        scalar(&mut a, "SELECT v FROM iso WHERE id = 1").await
    );

    // Writer deletes and re-inserts the same key — the round-493 churn.
    b.execute("DELETE FROM iso WHERE id = 1").await?;
    b.execute("INSERT INTO iso VALUES (1, 12)").await?;
    println!(
        "| after writer DELETE + re-INSERT | {} |",
        scalar(&mut a, "SELECT v FROM iso WHERE id = 1").await
    );

    // A row the reader never saw must stay invisible.
    b.execute("INSERT INTO iso VALUES (3, 30)").await?;
    println!(
        "| count after writer INSERTs a new row | {} |",
        scalar(&mut a, "SELECT count(*)::int FROM iso").await
    );

    a.execute("COMMIT").await?;
    println!(
        "| reader after COMMIT | {} |",
        scalar(&mut a, "SELECT v FROM iso WHERE id = 1").await
    );
    println!(
        "| reader, second read after COMMIT | {} |",
        scalar(&mut a, "SELECT v FROM iso WHERE id = 1").await
    );
    println!(
        "| reader count after COMMIT | {} |",
        scalar(&mut a, "SELECT count(*)::int FROM iso").await
    );
    a.execute("BEGIN").await?;
    println!(
        "| reader inside a fresh BEGIN | {} |",
        scalar(&mut a, "SELECT v FROM iso WHERE id = 1").await
    );
    a.execute("COMMIT").await?;
    println!(
        "| reader after that COMMIT | {} |",
        scalar(&mut a, "SELECT v FROM iso WHERE id = 1").await
    );
    // A connection that never opened a transaction, for contrast.
    let mut fresh = AnyConnection::connect(&url).await?;
    println!(
        "| a brand-new connection | {} |",
        scalar(&mut fresh, "SELECT v FROM iso WHERE id = 1").await
    );
    // And the writer's own view.
    println!(
        "| the writer itself | {} |",
        scalar(&mut b, "SELECT v FROM iso WHERE id = 1").await
    );
    println!();
    println!("# PG18: 10, 10, 10, 2, then 12 everywhere after COMMIT.");
    Ok(())
}
