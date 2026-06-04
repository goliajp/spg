//! Minimal end-to-end async example.
//!
//! ```text
//! cargo run -p spg-embedded-tokio --example hello_async
//! ```

use spg_embedded_tokio::{AsyncDatabase, EngineError, Value};

#[tokio::main]
async fn main() -> Result<(), EngineError> {
    let db = AsyncDatabase::open_in_memory();

    db.execute(
        "CREATE TABLE tasks (
            id BIGSERIAL PRIMARY KEY,
            title TEXT NOT NULL,
            done BOOL NOT NULL DEFAULT false
        )",
    )
    .await?;

    db.execute("INSERT INTO tasks (title) VALUES ('learn async SPG')")
        .await?;
    db.execute("INSERT INTO tasks (title, done) VALUES ('ship v7.10.0', true)")
        .await?;

    for row in db
        .query("SELECT id, title, done FROM tasks ORDER BY id")
        .await?
    {
        let Value::BigInt(id) = row[0] else {
            unreachable!()
        };
        let Value::Text(ref title) = row[1] else {
            unreachable!()
        };
        let Value::Bool(done) = row[2] else {
            unreachable!()
        };
        let marker = if done { "✓" } else { " " };
        println!("[{marker}] #{id} {title}");
    }

    Ok(())
}
