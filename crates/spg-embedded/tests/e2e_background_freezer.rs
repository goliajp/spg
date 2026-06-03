//! v7.2.1 — `Database::spawn_background_freezer` + the
//! `Arc<Mutex<Database>>` sharing pattern.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use spg_embedded::{Database, FreezerOptions};

#[test]
fn background_freezer_demotes_when_hot_tier_exceeds_budget() {
    let mut db = Database::open_in_memory();
    db.execute("CREATE TABLE t (id INT NOT NULL, payload TEXT NOT NULL)")
        .unwrap();
    db.execute("CREATE INDEX by_id ON t (id)").unwrap();
    for i in 0..200i64 {
        let payload = "x".repeat(40);
        db.execute(&format!("INSERT INTO t VALUES ({i}, '{payload}')"))
            .unwrap();
    }
    let shared = Arc::new(Mutex::new(db));
    let opts = FreezerOptions {
        tick: Duration::from_millis(50),
        hot_tier_bytes: 256,
        batch_rows: 32,
    };
    let _handle = Database::spawn_background_freezer(Arc::clone(&shared), opts);
    // Wait up to 5 s for the freezer to produce at least one
    // cold segment.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        std::thread::sleep(Duration::from_millis(100));
        let cold = shared.lock().unwrap().engine().catalog().cold_segment_count();
        if cold >= 1 {
            return;
        }
        if std::time::Instant::now() > deadline {
            panic!("background freezer never fired");
        }
    }
}

#[test]
fn freezer_handle_drop_stops_thread_cleanly() {
    // After the handle drops, the worker exits within a tick.
    // Sanity check: a second handle reuses the same Database
    // and fires again — no zombie state.
    let mut db = Database::open_in_memory();
    db.execute("CREATE TABLE t (id INT NOT NULL)").unwrap();
    db.execute("CREATE INDEX by_id ON t (id)").unwrap();
    for i in 0..50i64 {
        db.execute(&format!("INSERT INTO t VALUES ({i})")).unwrap();
    }
    let shared = Arc::new(Mutex::new(db));
    let opts = FreezerOptions {
        tick: Duration::from_millis(50),
        hot_tier_bytes: 32,
        batch_rows: 20,
    };
    {
        let _handle1 = Database::spawn_background_freezer(Arc::clone(&shared), opts.clone());
        // Wait a bit so the freezer has a chance to fire.
        std::thread::sleep(Duration::from_millis(500));
    }
    // Handle dropped → worker joined. Spawn a new one.
    let _handle2 = Database::spawn_background_freezer(Arc::clone(&shared), opts);
    std::thread::sleep(Duration::from_millis(500));
    // No assertion needed beyond "no panic / no deadlock".
}

#[test]
fn freezer_no_op_when_no_freezable_table() {
    // Empty catalog (or no BTree int-PK index) → freezer
    // happily ticks without firing.
    let db = Database::open_in_memory();
    let shared = Arc::new(Mutex::new(db));
    let opts = FreezerOptions {
        tick: Duration::from_millis(50),
        hot_tier_bytes: 1,
        batch_rows: 10,
    };
    let _h = Database::spawn_background_freezer(Arc::clone(&shared), opts);
    std::thread::sleep(Duration::from_millis(300));
    // The freezer didn't crash; cold_segment_count stays 0.
    assert_eq!(
        shared.lock().unwrap().engine().catalog().cold_segment_count(),
        0
    );
}
