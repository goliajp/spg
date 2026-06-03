//! Open a persistent DB, write rows, close, re-open, confirm
//! rows survived. `cargo run --example persistent /tmp/spg.db`.

use spg_embedded::Database;

fn main() -> Result<(), spg_embedded::EngineError> {
    let path = std::env::args().nth(1).expect("usage: persistent <db_path>");

    // First pass — create + insert.
    {
        let mut db = Database::open_path(&path)?;
        db.execute("CREATE TABLE IF NOT EXISTS events (
            id INT NOT NULL AUTO_INCREMENT,
            ts TIMESTAMP DEFAULT NOW(),
            payload TEXT
        )")?;
        db.execute("INSERT INTO events (payload) VALUES ('booted')")?;
        // Drop runs the destructor — WAL is already fsynced per
        // execute(), so this drop is just a clean handle close.
    }

    // Second pass — re-open, read.
    let mut db = Database::open_path(&path)?;
    let rows = db.query("SELECT id, ts, payload FROM events ORDER BY id")?;
    println!("Persisted {} rows:", rows.len());
    for row in &rows {
        println!("  {row:?}");
    }

    Ok(())
}
