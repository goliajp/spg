//! v7.39 (round 222) — LISTEN / NOTIFY with real delivery (was
//! accept-and-drop since v7.37.17). PG semantics pinned per live-PG18.4:
//! transactional delivery at COMMIT, dedup within one tx, dropped at
//! ROLLBACK, immediate under autocommit, only LISTENed channels deliver,
//! UNLISTEN * clears all. Embedded callers drain via
//! `Engine::take_notifications`; the wire layer turns each into an 'A'
//! NotificationResponse (pinned separately in spg-server e2e).

use spg_engine::Engine;

fn drained(e: &mut Engine) -> Vec<(String, String)> {
    e.take_notifications()
}

#[test]
fn autocommit_notify_delivers_to_listener() {
    let mut e = Engine::new();
    e.execute("LISTEN mychan").unwrap();
    e.execute("NOTIFY mychan, 'hello'").unwrap();
    assert_eq!(
        drained(&mut e),
        vec![("mychan".to_string(), "hello".to_string())]
    );
    // No payload → empty string (PG's default).
    e.execute("NOTIFY mychan").unwrap();
    assert_eq!(
        drained(&mut e),
        vec![("mychan".to_string(), String::new())]
    );
}

#[test]
fn non_listened_channel_drops() {
    let mut e = Engine::new();
    e.execute("LISTEN a").unwrap();
    e.execute("NOTIFY b, 'x'").unwrap();
    assert_eq!(drained(&mut e), Vec::<(String, String)>::new());
}

#[test]
fn tx_notify_delivers_at_commit_deduplicated() {
    let mut e = Engine::new();
    e.execute("LISTEN c").unwrap();
    e.execute("BEGIN").unwrap();
    e.execute("NOTIFY c, 'p'").unwrap();
    e.execute("NOTIFY c, 'p'").unwrap(); // dup within the tx → one delivery
    e.execute("NOTIFY c, 'q'").unwrap();
    // Nothing delivered before COMMIT.
    assert_eq!(drained(&mut e), Vec::<(String, String)>::new());
    e.execute("COMMIT").unwrap();
    assert_eq!(
        drained(&mut e),
        vec![
            ("c".to_string(), "p".to_string()),
            ("c".to_string(), "q".to_string()),
        ]
    );
}

#[test]
fn rollback_drops_pending_notifies() {
    let mut e = Engine::new();
    e.execute("LISTEN c").unwrap();
    e.execute("BEGIN").unwrap();
    e.execute("NOTIFY c, 'gone'").unwrap();
    e.execute("ROLLBACK").unwrap();
    assert_eq!(drained(&mut e), Vec::<(String, String)>::new());
}

#[test]
fn unlisten_stops_delivery() {
    let mut e = Engine::new();
    e.execute("LISTEN a").unwrap();
    e.execute("LISTEN b").unwrap();
    e.execute("UNLISTEN a").unwrap();
    e.execute("NOTIFY a, '1'").unwrap();
    e.execute("NOTIFY b, '2'").unwrap();
    assert_eq!(drained(&mut e), vec![("b".to_string(), "2".to_string())]);
    e.execute("UNLISTEN *").unwrap();
    e.execute("NOTIFY b, '3'").unwrap();
    assert_eq!(drained(&mut e), Vec::<(String, String)>::new());
}
