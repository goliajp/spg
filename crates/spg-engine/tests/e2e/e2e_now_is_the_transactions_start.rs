//! 8.0.3 — `now()` is the time the transaction began.
//!
//! SPG folded every clock function in a statement to one reading taken
//! when the statement was prepared, so inside a transaction `now()` moved
//! from statement to statement and equalled `statement_timestamp()`.
//! Reported by sentori; measured on PG 18.6, and identical on every SPG
//! build back to 7.38.6:
//!
//! ```text
//!                                                        PG 18.6   SPG 8.0.2
//!   now() in two statements of one transaction, same?    t         f
//!   now() = statement_timestamp() inside a transaction   f         t
//! ```
//!
//! PG's three clocks: `now()` / `current_timestamp` /
//! `transaction_timestamp()` read the moment the transaction BEGAN — the
//! BEGIN itself, not its first query (BEGIN, wait a second, and
//! `statement_timestamp() - now()` reads 1.0); `statement_timestamp()`
//! reads the start of the statement; `clock_timestamp()` reads the clock.
//! Outside a transaction the first two are one instant.
//!
//! The pins drive a clock they control, so every expectation is exact.

use core::sync::atomic::{AtomicI64, Ordering};

use spg_engine::{Engine, IMPLICIT_TX, QueryResult};

static CLOCK: AtomicI64 = AtomicI64::new(1_800_000_000_000_000);

fn clock() -> i64 {
    CLOCK.load(Ordering::SeqCst)
}

fn advance_seconds(s: i64) {
    CLOCK.fetch_add(s * 1_000_000, Ordering::SeqCst);
}

fn epoch(e: &mut Engine, tx: spg_engine::TxId, expr: &str) -> String {
    let sql = format!("SELECT extract(epoch from {expr})::bigint");
    match e
        .execute_in(&sql, tx)
        .unwrap_or_else(|x| panic!("{sql}: {x}"))
    {
        QueryResult::Rows { rows, .. } => spg_engine::eval::value_to_text(&rows[0].values[0]),
        other => panic!("{other:?}"),
    }
}

/// One test body, so the shared clock is advanced in a known order.
#[test]
fn the_three_clocks_read_what_postgresql_reads() {
    let mut e = Engine::new().with_clock(clock);
    let begin_at = clock() / 1_000_000;

    let tx = e.alloc_tx_id();
    e.execute_in("BEGIN", tx).unwrap();
    advance_seconds(1);
    let first = epoch(&mut e, tx, "now()");
    advance_seconds(1);
    let second = epoch(&mut e, tx, "now()");
    assert_eq!(
        first,
        begin_at.to_string(),
        "now() is the BEGIN, not the first query"
    );
    assert_eq!(first, second, "and it does not move inside the transaction");
    assert_eq!(epoch(&mut e, tx, "transaction_timestamp()"), first);
    assert_eq!(epoch(&mut e, tx, "current_timestamp"), first);
    assert_eq!(
        epoch(&mut e, tx, "statement_timestamp()"),
        (begin_at + 2).to_string(),
        "statement_timestamp() is this statement's start"
    );
    e.execute_in("COMMIT", tx).unwrap();

    // Outside a transaction, every statement is its own transaction.
    advance_seconds(5);
    let auto = epoch(&mut e, IMPLICIT_TX, "now()");
    assert_eq!(
        auto,
        (begin_at + 7).to_string(),
        "autocommit now() is the statement's own time"
    );
    assert_eq!(epoch(&mut e, IMPLICIT_TX, "statement_timestamp()"), auto);

    // A new transaction takes a new start.
    let tx2 = e.alloc_tx_id();
    e.execute_in("BEGIN", tx2).unwrap();
    advance_seconds(3);
    assert_eq!(epoch(&mut e, tx2, "now()"), (begin_at + 7).to_string());
    e.execute_in("ROLLBACK", tx2).unwrap();
}
