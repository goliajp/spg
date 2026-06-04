//! Foreign keys with ON DELETE CASCADE. New in v7.6.
//! `cargo run --example foreign_keys`.

use spg_embedded::{Database, EngineError};

fn main() -> Result<(), EngineError> {
    let mut db = Database::open_in_memory();

    db.execute("CREATE TABLE customers (id INT NOT NULL)")?;
    db.execute("CREATE INDEX customers_pk ON customers (id)")?;

    db.execute(
        "CREATE TABLE orders (
        id INT NOT NULL AUTO_INCREMENT,
        customer_id INT NOT NULL,
        FOREIGN KEY (customer_id) REFERENCES customers(id) ON DELETE CASCADE
    )",
    )?;

    db.execute("INSERT INTO customers VALUES (1), (2)")?;
    db.execute("INSERT INTO orders (customer_id) VALUES (1), (1), (2)")?;

    println!("Before delete:");
    print_counts(&mut db)?;

    // Removing customer 1 cascades into their orders.
    db.execute("DELETE FROM customers WHERE id = 1")?;

    println!("After DELETE FROM customers WHERE id = 1:");
    print_counts(&mut db)?;

    Ok(())
}

fn print_counts(db: &mut Database) -> Result<(), EngineError> {
    for table in ["customers", "orders"] {
        let rows = db.query(&format!("SELECT id FROM {table}"))?;
        println!("  {table:10}: {} rows", rows.len());
    }
    Ok(())
}
