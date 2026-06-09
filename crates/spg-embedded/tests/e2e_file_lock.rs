//! v7.17.0 Phase 6.2 — cross-process exclusion lock on
//! Database::open_path.

use spg_embedded::Database;
use spg_engine::EngineError;

fn tmpdb(label: &str) -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("spg-lock-{label}-{nanos}"));
    std::fs::create_dir_all(&dir).unwrap();
    dir.join("spg.db")
}

#[test]
fn first_open_acquires_lock() {
    let p = tmpdb("first");
    let _db = Database::open_path(&p).unwrap();
    let mut lock_path = p.clone();
    let name = lock_path.file_name().unwrap().to_os_string();
    let mut s = name.clone();
    s.push(".lock");
    lock_path.set_file_name(s);
    assert!(lock_path.exists(), "lock dir should exist after open");
}

#[test]
fn second_open_to_same_path_fails() {
    let p = tmpdb("second");
    let _db = Database::open_path(&p).unwrap();
    let r = Database::open_path(&p);
    match r {
        Err(EngineError::Unsupported(msg)) => {
            assert!(
                msg.contains("locked"),
                "expected lock-related error, got: {msg}"
            );
        }
        other => panic!("expected lock error, got {other:?}"),
    }
}

#[test]
fn drop_releases_lock() {
    let p = tmpdb("release");
    {
        let _db = Database::open_path(&p).unwrap();
    }
    let mut lock_path = p.clone();
    let name = lock_path.file_name().unwrap().to_os_string();
    let mut s = name.clone();
    s.push(".lock");
    lock_path.set_file_name(s);
    assert!(!lock_path.exists(), "lock dir should be removed on Drop");
    // Re-open should succeed.
    let _db2 = Database::open_path(&p).unwrap();
}

#[test]
fn force_unlock_clears_stale_lock() {
    let p = tmpdb("force");
    // Manually create a stale lock dir.
    let mut lock_path = p.clone();
    let name = lock_path.file_name().unwrap().to_os_string();
    let mut s = name.clone();
    s.push(".lock");
    lock_path.set_file_name(s);
    std::fs::create_dir_all(&lock_path).unwrap();
    // Now confirm open fails…
    assert!(Database::open_path(&p).is_err());
    // …and force_unlock clears it.
    Database::force_unlock(&p).unwrap();
    assert!(!lock_path.exists());
    // …and a clean open now succeeds.
    let _db = Database::open_path(&p).unwrap();
}

#[test]
fn force_unlock_on_missing_lock_is_noop() {
    let p = tmpdb("noop");
    // No lock present — should succeed silently.
    Database::force_unlock(&p).unwrap();
}

#[test]
fn open_in_memory_does_not_acquire_lock() {
    // In-memory databases have no persistence path, so no
    // lock to acquire.
    let _db = Database::open_in_memory();
    // No way to assert "no lock" without a path; the absence of
    // panics / errors is enough.
}
