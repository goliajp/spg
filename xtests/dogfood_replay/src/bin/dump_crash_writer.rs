//! v7.38.7 — the child half of the `dump-crash-recovery` fixture.
//!
//! Restores a dump into a data directory, acknowledges a burst of
//! writes, and then dies the way a machine dies: `SIGKILL` to itself,
//! with no unwinding, no Drop, no flush. The parent reopens the same
//! directory and checks what survived.
//!
//! This is a separate process because that is the only honest way to
//! test it — a harness cannot `kill -9` itself and carry on, and
//! anything softer (an abort, a panic, a dropped handle) exercises a
//! shutdown path that a power cut does not take.
//!
//! Argv: <db> <dump.sql> <statement> <kill_after>
//! `{i}` in the statement is replaced by the iteration number.
//!
//! It prints the acknowledged count to stdout and flushes BEFORE
//! killing itself, so the parent knows exactly how many writes the
//! client was told had committed. That number is the contract: every
//! one of them must be there after the reopen.

// The SIGKILL below is the whole point of this binary, so the crate's
// deny-unsafe posture is lifted here and nowhere else.
#![allow(unsafe_code)]

use std::io::Write as _;

fn main() {
    let a: Vec<String> = std::env::args().collect();
    assert!(a.len() >= 5, "argv: <db> <dump> <stmt> <n>");
    let (db, dump, stmt) = (&a[1], &a[2], &a[3]);
    let kill_after: u32 = a[4].parse().expect("kill_after");

    let mut d = spg_embedded::Database::open_path(db).expect("open");
    let sql = std::fs::read_to_string(dump).expect("read dump");
    // pg_dump output is statement-per-unit; the script runner handles
    // the multi-statement form the same way psql does.
    d.execute_script(&sql).expect("restore dump");

    let mut acked = 0u32;
    for i in 0..kill_after {
        d.execute(&stmt.replace("{i}", &i.to_string()))
            .unwrap_or_else(|e| panic!("write {i}: {e}"));
        acked += 1;
    }
    println!("ACKED {acked}");
    std::io::stdout().flush().expect("flush");

    // The kill. Not abort(), not panic!() — those run handlers a power
    // cut would not.
    unsafe {
        libc::raise(libc::SIGKILL);
    }
    unreachable!("SIGKILL did not land");
}
