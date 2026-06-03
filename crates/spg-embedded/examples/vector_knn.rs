//! Vector similarity search with HNSW. pgvector-style ops.
//! `cargo run --example vector_knn`.

use spg_embedded::{Database, EngineError};

fn main() -> Result<(), EngineError> {
    let mut db = Database::open_in_memory();
    db.execute("CREATE TABLE docs (id INT NOT NULL, emb VECTOR(3) NOT NULL)")?;
    db.execute("CREATE INDEX docs_emb ON docs USING hnsw (emb)")?;

    db.execute("INSERT INTO docs VALUES \
        (1, [1.0, 0.0, 0.0]), \
        (2, [0.0, 1.0, 0.0]), \
        (3, [0.0, 0.0, 1.0]), \
        (4, [0.9, 0.1, 0.0]) \
    ")?;

    // L2 distance — the row closest to [1,0,0] is row 1; row 4
    // is second; rows 2 and 3 are equidistant further out.
    let rows = db.query(
        "SELECT id FROM docs ORDER BY emb <-> [1.0, 0.0, 0.0] LIMIT 2",
    )?;
    for row in rows {
        println!("{row:?}");
    }
    Ok(())
}
