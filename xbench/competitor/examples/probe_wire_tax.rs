//! r176 scratch probe — split the SPGS extended-protocol ~4ms/stmt tax.
//!
//! Round 1 found the panel's 4ms does NOT reproduce with the native
//! sqlx Pg driver on an empty table (A=0.218ms). Round 2 adds the two
//! variables the wire_heavy panel had: the sqlx **Any** driver and a
//! 50k-row seeded table with PK + secondary index.
//!
//! Arms (all session synchronous_commit=off, same server):
//!   A: Pg driver, unique SQL, empty-ish table p
//!   D: Any driver, unique SQL, table p            (driver effect)
//!   E: Pg driver, unique SQL, 50k-seeded table wb (table-size effect)
//!   F: Any driver, unique SQL, 50k-seeded table wb (panel shape)
//! Run: SPGS_URL=... cargo run --release -p spg-bench-competitor --example probe_wire_tax

use std::fmt::Write as _;
use std::time::Instant;

async fn seed_wb(pool: &sqlx::PgPool) -> Result<(), sqlx::Error> {
    sqlx::query("CREATE TABLE wb (id INT PRIMARY KEY, g INT NOT NULL, v INT NOT NULL)")
        .execute(pool)
        .await?;
    sqlx::query("CREATE INDEX wb_v_idx ON wb (v)")
        .execute(pool)
        .await?;
    for base in 0..50 {
        let mut sql = String::with_capacity(1000 * 24 + 32);
        sql.push_str("INSERT INTO wb VALUES ");
        for k in 0..1000 {
            let id = base * 1000 + k + 1;
            if k > 0 {
                sql.push(',');
            }
            let _ = write!(sql, "({id}, {}, {})", id % 100, (id * 7) % 100_000);
        }
        sqlx::query(&sql).execute(pool).await?;
    }
    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    sqlx::any::install_default_drivers();
    let url = std::env::var("SPGS_URL")?;
    let pg = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .connect(&url)
        .await?;
    sqlx::query("CREATE TABLE p (id INT PRIMARY KEY, g INT NOT NULL)")
        .execute(&pg)
        .await?;
    sqlx::query("SET synchronous_commit = off")
        .execute(&pg)
        .await?;

    // A: Pg driver, unique SQL, small table.
    let t0 = Instant::now();
    for i in 0..100 {
        sqlx::query(&format!("INSERT INTO p VALUES ({i}, 0)"))
            .execute(&pg)
            .await?;
    }
    let a = t0.elapsed().as_secs_f64() * 1000.0;

    // D: Any driver, unique SQL, small table.
    let any = sqlx::any::AnyPoolOptions::new()
        .max_connections(1)
        .connect(&url)
        .await?;
    sqlx::query("SET synchronous_commit = off")
        .execute(&any)
        .await?;
    let t0 = Instant::now();
    for i in 1000..1100 {
        sqlx::query(&format!("INSERT INTO p VALUES ({i}, 0)"))
            .execute(&any)
            .await?;
    }
    let d = t0.elapsed().as_secs_f64() * 1000.0;

    // E: Pg driver, unique SQL, 50k-seeded wb.
    seed_wb(&pg).await?;
    let t0 = Instant::now();
    for i in 2_000_000..2_000_100 {
        sqlx::query(&format!("INSERT INTO wb VALUES ({i}, 0, 0)"))
            .execute(&pg)
            .await?;
    }
    let e = t0.elapsed().as_secs_f64() * 1000.0;

    // F: Any driver, unique SQL, 50k-seeded wb (the panel shape).
    let t0 = Instant::now();
    for i in 3_000_000..3_000_100 {
        sqlx::query(&format!("INSERT INTO wb VALUES ({i}, 0, 0)"))
            .execute(&any)
            .await?;
    }
    let f = t0.elapsed().as_secs_f64() * 1000.0;

    println!("A pg  small : {a:8.1} ms  {:.3} ms/stmt", a / 100.0);
    println!("D any small : {d:8.1} ms  {:.3} ms/stmt", d / 100.0);
    println!("E pg  50k   : {e:8.1} ms  {:.3} ms/stmt", e / 100.0);
    println!("F any 50k   : {f:8.1} ms  {:.3} ms/stmt", f / 100.0);
    Ok(())
}
