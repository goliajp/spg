//! v7.39 (round 494) — does a second connection's write survive while a
//! first connection holds a transaction open?
//!
//! `iso_cross_session` showed SPGS matching PG on the isolation itself and
//! then diverging on the last line: after the reader COMMITs, PG sees the
//! writer's value and SPGS sees the old one — and so does a brand-new
//! connection, and so does the writer itself. That is not stale reading;
//! it says the writer's statements had no effect at all. This narrows it.
//!
//!   ISO_URL=postgres://…  cargo run --release --bin iso_lost_write

use sqlx::{AnyConnection, Connection as _, Executor as _, Row as _};

async fn v(c: &mut AnyConnection) -> String {
    match c.fetch_all("SELECT v FROM iso2 WHERE id = 1").await {
        Ok(rows) => rows
            .first()
            .and_then(|r| r.try_get::<i32, _>(0).ok())
            .map_or_else(|| "<gone>".into(), |x| x.to_string()),
        Err(e) => format!("ERR {e}"),
    }
}

async fn setup(c: &mut AnyConnection) -> Result<(), sqlx::Error> {
    c.execute("DROP TABLE IF EXISTS iso2").await?;
    c.execute("CREATE TABLE iso2 (id INT PRIMARY KEY, v INT)").await?;
    c.execute("INSERT INTO iso2 VALUES (1, 10)").await?;
    Ok(())
}

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    sqlx::any::install_default_drivers();
    let url = std::env::var("ISO_URL").map_err(|_| "set ISO_URL")?;
    println!("| case | writer's view | a third connection | after the holder COMMITs |");
    println!("|------|-------------:|-------------------:|-------------------------:|");

    for case in ["no other transaction", "other holds READ COMMITTED", "other holds REPEATABLE READ"] {
        let mut a = AnyConnection::connect(&url).await?;
        let mut b = AnyConnection::connect(&url).await?;
        setup(&mut a).await?;
        match case {
            "other holds READ COMMITTED" => {
                a.execute("BEGIN").await?;
                let _ = v(&mut a).await;
            }
            "other holds REPEATABLE READ" => {
                a.execute("BEGIN ISOLATION LEVEL REPEATABLE READ").await?;
                let _ = v(&mut a).await;
            }
            _ => {}
        }
        b.execute("UPDATE iso2 SET v = 99 WHERE id = 1").await?;
        let writer_view = v(&mut b).await;
        let mut c = AnyConnection::connect(&url).await?;
        let third = v(&mut c).await;
        let after_holder_commit = if case == "no other transaction" {
            "n/a".to_string()
        } else {
            a.execute("COMMIT").await?;
            let mut d = AnyConnection::connect(&url).await?;
            v(&mut d).await
        };
        println!("| {case} | {writer_view} | {third} | {after_holder_commit} |");
    }
    println!();
    println!("# PG18 answers 99 everywhere: another session's transaction —");
    println!("# including its COMMIT — cannot undo a write that already committed.");
    Ok(())
}
