//! v7.39 (round 637) — the trigger functions answered NULL where PG refuses.
//!
//! This round was still after `pg_proc`, and enumerating what SPG implements
//! turned up something first: **549 of the dispatcher's 1145 names accept a
//! zero-argument call**. The naive reading — probe both engines, diff
//! accept against reject — said 442 of those are calls PG refuses. That
//! number was wrong by 18x, and how it was wrong is the useful part:
//!
//!     SPG accepts at zero args   549
//!     PG accepts at zero args    108   <- genuinely nullary
//!     PG has no such function    988   <- SPG's own, not a divergence
//!     PG has it and refuses       49
//!
//! A probe has to tell three kinds of "no" apart: the function does not
//! exist, the signature does not match, and the runtime state forbids it.
//! Only the second is a signature divergence. Of the 24 that survived that
//! filter, most are the third kind (`pg_promote()` — "recovery is not in
//! progress") or a keyword mistaken for a function (`current_user()` — PG
//! answers `syntax error at or near "("`, because it is not a function).
//!
//! What was left is real: seven functions that only mean anything inside a
//! trigger, answering NULL when called as a scalar. The code said so —
//! "PG errors outside that context, SPG returns NULL to stay parse-through
//! for tooling" — and the tooling argument cuts the other way, since a tool
//! probing PG gets the error. A tool reading NULL from
//! `pg_event_trigger_ddl_commands()` concludes there were no DDL commands.
//!
//! Messages measured from PG18, each naming the manager or trigger kind it
//! needs.

use spg_engine::Engine;

fn err(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql) {
        Err(err) => err.to_string(),
        Ok(ok) => panic!("{sql}: expected a rejection, got {ok:?}"),
    }
}

#[test]
fn round637_trigger_functions_refuse_a_scalar_call() {
    let mut e = Engine::new();
    for (sql, want) in [
        (
            "SELECT tsvector_update_trigger()",
            "tsvector_update_trigger: not fired by trigger manager",
        ),
        (
            "SELECT tsvector_update_trigger_column()",
            "tsvector_update_trigger: not fired by trigger manager",
        ),
        (
            "SELECT suppress_redundant_updates_trigger()",
            "suppress_redundant_updates_trigger: must be called as trigger",
        ),
    ] {
        let m = err(&mut e, sql);
        assert!(m.ends_with(want), "{sql}: wanted {want:?}, said {m:?}");
    }
}

#[test]
fn round637_event_trigger_readers_name_the_trigger_kind() {
    let mut e = Engine::new();
    for (sql, want) in [
        (
            "SELECT pg_event_trigger_ddl_commands()",
            "pg_event_trigger_ddl_commands() can only be called in an event trigger function",
        ),
        (
            "SELECT pg_event_trigger_dropped_objects()",
            "pg_event_trigger_dropped_objects() can only be called in a sql_drop event trigger function",
        ),
        (
            "SELECT pg_event_trigger_table_rewrite_oid()",
            "pg_event_trigger_table_rewrite_oid() can only be called in a table_rewrite event trigger function",
        ),
        (
            "SELECT pg_event_trigger_table_rewrite_reason()",
            "pg_event_trigger_table_rewrite_reason() can only be called in a table_rewrite event trigger function",
        ),
    ] {
        let m = err(&mut e, sql);
        assert!(m.ends_with(want), "{sql}: wanted {want:?}, said {m:?}");
    }
    // Not every stub becomes an error: PG has no context requirement on
    // this one, and the binary-upgrade setters do not exist in PG at all,
    // so their NULL has nothing to disagree with.
    assert!(e.execute("SELECT pg_listening_channels()").is_ok());
    assert!(
        e.execute("SELECT binary_upgrade_set_next_pg_type_oid(16384)")
            .is_ok()
    );
}
