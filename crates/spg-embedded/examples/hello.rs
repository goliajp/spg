//! 30-second tour. `cargo run --example hello`.

use spg_embedded::Database;

fn main() -> Result<(), spg_embedded::EngineError> {
    let mut db = Database::open_in_memory();
    db.execute("CREATE TABLE users (id INT NOT NULL, name TEXT)")?;
    db.execute("INSERT INTO users VALUES (1, 'alice'), (2, 'bob')")?;
    let rows = db.query("SELECT id, name FROM users ORDER BY id")?;
    for row in rows {
        println!("{row:?}");
    }
    Ok(())
}
