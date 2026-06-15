//! v7.33 (P4 borrow channel, increment 3) — the projection borrow
//! channel never-die gate. A non-aggregate, high-row JOIN projection
//! (`SELECT a.col, b.col … FROM a JOIN b`) must NOT rebuild an
//! intermediate combined `Row` per survivor and clone every cell twice
//! (source → materialised → output). `exec_joined_select` projects
//! straight off the deferred row-index tuples, reading bound qualified
//! columns by reference and cloning once into the output row.
//!
//! Measured allocation count (release, M-series): 11.0/row before the
//! increment-3 change, 7.0/row after — a flat 100k fewer allocations on
//! this 25k-row shape (the intermediate `materialise()` Vec + its
//! per-cell source→intermediate clones). The 7/row floor is the output
//! Row Vec + the three owned String cells the result must own. The
//! budget below (9/row) keeps headroom over 7 while a regression back to
//! the 11/row materialise-then-project shape trips it.
//!
//! Wall-clock is load-sensitive (not asserted here); the alloc count is
//! deterministic and machine-independent, so this gate runs in the fast
//! tier.

use crate::{ALLOC_CALLS, perf_lock};
use spg_engine::{Engine, QueryResult};
use std::sync::atomic::Ordering;

const ROWS: usize = 25_000;
/// 7.0/row measured post-fix; trips on a regression to the 11/row
/// materialise-then-project shape.
const ALLOC_PER_ROW_BUDGET: usize = 9;

fn seed(db: &mut Engine) {
    db.execute("CREATE TABLE mailboxes (id BIGSERIAL PRIMARY KEY, name TEXT, user_address TEXT)")
        .unwrap();
    db.execute("CREATE TABLE messages (id BIGSERIAL PRIMARY KEY, mailbox_id BIGINT, thread_id TEXT, subject TEXT, sender TEXT, internal_date BIGINT)").unwrap();
    for i in 0..30 {
        db.execute(&format!(
            "INSERT INTO mailboxes (name, user_address) VALUES ('mb{i}', 'u@x')"
        ))
        .unwrap();
    }
    for batch in 0..(ROWS / 500) {
        let mut vals = Vec::new();
        for j in 0..500 {
            let i = batch * 500 + j;
            vals.push(format!(
                "({}, 'th-{i}', 'subject {i}', 's{}@x', {})",
                (i % 30) + 1,
                i % 100,
                1_700_000_000 + i
            ));
        }
        db.execute(&format!(
            "INSERT INTO messages (mailbox_id, thread_id, subject, sender, internal_date) VALUES {}",
            vals.join(",")
        ))
        .unwrap();
    }
}

// Bound qualified columns, no ORDER BY — the borrow-channel fast path.
const PROJ: &str = "SELECT m.id, m.subject, m.sender, m.internal_date, mb.user_address \
    FROM messages m JOIN mailboxes mb ON m.mailbox_id = mb.id WHERE mb.user_address = 'u@x'";

#[test]
fn join_projection_stays_on_the_borrow_channel() {
    let _g = perf_lock();
    let mut db = Engine::new();
    seed(&mut db);
    db.execute(PROJ).unwrap(); // warm (plan cache / allocator)

    let a0 = ALLOC_CALLS.load(Ordering::Relaxed);
    let r = db.execute(PROJ).unwrap();
    let allocs = ALLOC_CALLS.load(Ordering::Relaxed) - a0;

    let n = match r {
        QueryResult::Rows { rows, .. } => rows.len(),
        other => panic!("unexpected result: {other:?}"),
    };
    assert_eq!(n, ROWS, "projection must return every joined row");
    let per_row = allocs as f64 / n as f64;
    assert!(
        per_row <= ALLOC_PER_ROW_BUDGET as f64,
        "join projection allocated {per_row:.1}/row ({allocs} for {n} rows) — \
         budget {ALLOC_PER_ROW_BUDGET}/row; the materialise-then-project \
         intermediate Row clone is back (was 11/row pre-increment-3)"
    );
}
