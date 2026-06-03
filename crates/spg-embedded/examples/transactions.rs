//! Bulk-load via `with_transaction`. One fsync per batch
//! instead of one per row. `cargo run --example transactions`.

use spg_embedded::{Database, EngineError};

fn main() -> Result<(), EngineError> {
    let mut db = Database::open_in_memory();
    db.execute("CREATE TABLE ledger (
        id INT NOT NULL AUTO_INCREMENT,
        account TEXT NOT NULL,
        delta_cents BIGINT NOT NULL
    )")?;

    let entries = [
        ("alice",  10_000),
        ("alice",  -2_500),
        ("bob",     1_000),
        ("bob",     5_000),
        ("alice", -1_500),
    ];

    db.with_transaction(|tx| {
        for (acct, delta) in entries {
            let sql = format!(
                "INSERT INTO ledger (account, delta_cents) VALUES ('{acct}', {delta})"
            );
            tx.execute(&sql)?;
        }
        Ok::<_, EngineError>(())
    })?;

    let totals = db.query("
        SELECT account, SUM(delta_cents)
        FROM ledger
        GROUP BY account
        ORDER BY account
    ")?;
    for row in totals {
        println!("{row:?}");
    }
    Ok(())
}
