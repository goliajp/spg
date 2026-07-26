//! v7.39 (round 498) — is an advisory lock exclusive across sessions?
//!
//! `iso_session` T6 measured `pg_try_advisory_lock(4981)` answering true
//! on BOTH connections, where PG gives true then false — and B could even
//! "unlock" a lock it never took. The registry in `Engine::advisory_try_lock`
//! keys on `current_session` and looks right, and the two connections do
//! have distinct backend pids, so this asks the engine directly.
//!
//!   cargo run --release --example probe_advisory

use spg_engine::{Engine, QueryResult};

fn one(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql) {
        Ok(QueryResult::Rows { rows, .. }) => rows
            .first()
            .map(|r| spg_engine::eval::value_to_text(&r.values[0]))
            .unwrap_or_else(|| "<no rows>".into()),
        Ok(other) => format!("{other:?}"),
        Err(e) => format!("ERR {e}"),
    }
}

fn main() {
    let mut e = Engine::new();
    e.set_current_session(1);
    println!("A take   : {}", one(&mut e, "SELECT pg_try_advisory_lock(4981)"));
    e.set_current_session(2);
    println!("B take   : {} (PG: false)", one(&mut e, "SELECT pg_try_advisory_lock(4981)"));
    println!("B unlock : {} (PG: false)", one(&mut e, "SELECT pg_advisory_unlock(4981)"));
    e.set_current_session(1);
    println!("A take#2 : {} (PG: true, re-entrant)", one(&mut e, "SELECT pg_try_advisory_lock(4981)"));
    println!("A unlock : {} (PG: true)", one(&mut e, "SELECT pg_advisory_unlock(4981)"));
}
