//! Typed-row API via `spg_row!`. No proc-macro, no
//! `#[derive(...)]`. `cargo run --example typed`.

use spg_embedded::{Database, spg_row};

spg_row! {
    pub struct Product {
        pub id: i32,
        pub name: String,
        pub price_cents: Option<i64>,
    }
}

fn main() -> Result<(), spg_embedded::EngineError> {
    let mut db = Database::open_in_memory();
    db.execute("CREATE TABLE products (
        id INT NOT NULL,
        name TEXT NOT NULL,
        price_cents BIGINT
    )")?;
    db.execute("INSERT INTO products VALUES (1, 'widget', 1999), (2, 'gizmo', NULL)")?;

    let products: Vec<Product> = db.query_typed("SELECT id, name, price_cents FROM products")?;
    for p in &products {
        match p.price_cents {
            Some(n) => println!("#{}  {}  ${:.2}", p.id, p.name, n as f64 / 100.0),
            None => println!("#{}  {}  (not for sale)", p.id, p.name),
        }
    }
    Ok(())
}
