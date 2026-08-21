//! Same query, with the GIN index and without it. `to_tsvector(body) @@
//! to_tsquery('lazy')` over a row whose body is 'jumps over the lazy dog'
//! is one row in PostgreSQL 18.
use spg_engine::{Engine, QueryResult};

fn main() {
    for with_index in [false, true] {
        let mut e = Engine::new();
        e.execute("CREATE TABLE d (id INT, body TEXT)").unwrap();
        e.execute("INSERT INTO d VALUES (1,'jumps over the lazy dog'),(2,'sits')")
            .unwrap();
        if with_index {
            e.execute("CREATE INDEX g ON d USING gin (to_tsvector('english', body))")
                .unwrap();
        }
        let q =
            "SELECT id FROM d WHERE to_tsvector('english', body) @@ to_tsquery('english','lazy')";
        let n = match e.execute(q).unwrap() {
            QueryResult::Rows { rows, .. } => rows.len(),
            _ => unreachable!(),
        };
        println!("index={with_index:<5} -> {n} row(s)   (PG18 says 1)");
    }
}
