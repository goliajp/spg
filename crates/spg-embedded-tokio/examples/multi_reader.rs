//! Fan-out reader pattern with `AsyncReadHandle`.
//!
//! ```text
//! cargo run -p spg-embedded-tokio --example multi_reader
//! ```
//!
//! 16 concurrent readers query the same snapshot while a writer
//! task hammers the engine with INSERTs. The readers never block
//! on the writer's lock — they go through their own snapshot.

use spg_embedded_tokio::{AsyncDatabase, EngineError, Value};

#[tokio::main]
async fn main() -> Result<(), EngineError> {
    let db = AsyncDatabase::open_in_memory();

    db.execute("CREATE TABLE events (id INT NOT NULL, body TEXT NOT NULL)")
        .await?;
    for i in 0..1_000 {
        db.execute(&format!("INSERT INTO events VALUES ({i}, 'event-{i}')"))
            .await?;
    }

    // Concurrent writer keeps appending while readers fan out.
    let writer_db = db.clone();
    let writer = tokio::spawn(async move {
        for i in 1_000..2_000 {
            writer_db
                .execute(&format!("INSERT INTO events VALUES ({i}, 'event-{i}')"))
                .await
                .unwrap();
        }
    });

    // 16 fan-out readers, each on its own snapshot. They all see
    // the 1_000-row state from before the writer task started
    // (snapshots are taken sequentially before the writer fires —
    // in production take them from inside each task).
    let mut readers = Vec::new();
    for r in 0..16 {
        let h = db.read_handle().await;
        readers.push(tokio::spawn(async move {
            let rows = h.query("SELECT COUNT(*) FROM events").await.unwrap();
            let spg_embedded_tokio::QueryResult::Rows { rows, .. } = rows else {
                unreachable!()
            };
            let count = match &rows[0].values[0] {
                Value::BigInt(n) => *n,
                Value::Int(n) => i64::from(*n),
                other => panic!("unexpected: {other:?}"),
            };
            (r, count)
        }));
    }

    for r in readers {
        let (id, count) = r.await.expect("reader");
        println!("reader-{id:02}: SELECT COUNT(*) = {count}");
    }

    writer.await.expect("writer");

    // Final state — fresh handle picks up the new rows.
    let fresh = db.read_handle().await;
    let rows = fresh.query("SELECT COUNT(*) FROM events").await?;
    let spg_embedded_tokio::QueryResult::Rows { rows, .. } = rows else {
        unreachable!()
    };
    let final_count = match &rows[0].values[0] {
        Value::BigInt(n) => *n,
        Value::Int(n) => i64::from(*n),
        other => panic!("unexpected: {other:?}"),
    };
    println!("after writer: fresh COUNT(*) = {final_count}");

    Ok(())
}
