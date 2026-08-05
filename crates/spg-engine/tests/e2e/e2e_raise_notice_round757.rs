//! Round 757 (F31-B3) — `RAISE NOTICE / WARNING / INFO` deliver.
//! The round-753 audit found the executor discarding every non-
//! exception RAISE since v7.12.6 ("log to stderr … polish item" — it
//! did not even do that). PG18-measured over the wire (round-757
//! differential, six scenarios identical): NOTICE and WARNING honour
//! `client_min_messages`, INFO passes unconditionally, LOG and DEBUG
//! stay server-side, and a trigger body's RAISE sees NEW.

use spg_engine::{Engine, NoticeSeverity};

fn drained(e: &mut Engine, sql: &str) -> Vec<(NoticeSeverity, String)> {
    e.execute(sql).unwrap();
    e.take_notices()
        .into_iter()
        .map(|n| (n.severity, n.message))
        .collect()
}

#[test]
fn round757_do_block_raises_reach_the_notice_queue() {
    let mut e = Engine::new();
    assert_eq!(
        drained(&mut e, "DO $$ BEGIN RAISE NOTICE 'n %', 1; END $$"),
        [(NoticeSeverity::Notice, "n 1".to_string())]
    );
    assert_eq!(
        drained(&mut e, "DO $$ BEGIN RAISE WARNING 'w %', 2; END $$"),
        [(NoticeSeverity::Warning, "w 2".to_string())]
    );
    assert_eq!(
        drained(&mut e, "DO $$ BEGIN RAISE INFO 'i'; END $$"),
        [(NoticeSeverity::Info, "i".to_string())]
    );
    // LOG / DEBUG are server-log levels; nothing reaches the client.
    assert!(drained(&mut e, "DO $$ BEGIN RAISE LOG 'l'; END $$").is_empty());
    assert!(drained(&mut e, "DO $$ BEGIN RAISE DEBUG 'd'; END $$").is_empty());
}

#[test]
fn round757_client_min_messages_gates_notice_not_info() {
    let mut e = Engine::new();
    e.execute("SET client_min_messages = warning").unwrap();
    assert_eq!(
        drained(
            &mut e,
            "DO $$ BEGIN RAISE NOTICE 'gated'; RAISE INFO 'passes'; END $$"
        ),
        [(NoticeSeverity::Info, "passes".to_string())],
        "PG sends INFO regardless of client_min_messages"
    );
}

#[test]
fn round757_trigger_raise_reaches_the_notice_queue() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE b3t (id INT)").unwrap();
    e.execute(
        "CREATE FUNCTION b3fn() RETURNS trigger AS $$ BEGIN \
         RAISE NOTICE 'trig saw %', NEW.id; RETURN NEW; END $$ LANGUAGE plpgsql",
    )
    .unwrap();
    e.execute("CREATE TRIGGER b3trg BEFORE INSERT ON b3t FOR EACH ROW EXECUTE FUNCTION b3fn()")
        .unwrap();
    assert_eq!(
        drained(&mut e, "INSERT INTO b3t VALUES (7)"),
        [(NoticeSeverity::Notice, "trig saw 7".to_string())]
    );
}
