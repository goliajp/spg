//! v7.17.0 Phase 6.2 — cross-process exclusion lock on
//! Database::open_path.

use spg_embedded::Database;
use spg_engine::EngineError;

fn tmpdb(label: &str) -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir()
        .join("spg-tests")
        .join(format!("spg-lock-{label}-{nanos}"));
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
fn force_unlock_clears_live_lock() {
    let p = tmpdb("force");
    // Manually create a lock dir owned by a LIVE pid (our own) —
    // the round-12 liveness probe must treat it as held.
    let mut lock_path = p.clone();
    let name = lock_path.file_name().unwrap().to_os_string();
    let mut s = name.clone();
    s.push(".lock");
    lock_path.set_file_name(s);
    std::fs::create_dir_all(&lock_path).unwrap();
    std::fs::write(lock_path.join("pid"), std::process::id().to_string()).unwrap();
    // Now confirm open fails…
    assert!(Database::open_path(&p).is_err());
    // …and force_unlock clears it.
    Database::force_unlock(&p).unwrap();
    assert!(!lock_path.exists());
    // …and a clean open now succeeds.
    let _db = Database::open_path(&p).unwrap();
}

#[test]
fn stale_lock_without_owner_is_reclaimed() {
    // Round-12: a lock dir with no recorded owner pid (crash between
    // mkdir and pid write, or a pre-round-12 leftover) is stale by
    // definition — open reclaims it instead of erroring (a SIGKILL'd
    // mail server must not need manual lock surgery to restart).
    let p = tmpdb("stale");
    let mut lock_path = p.clone();
    let name = lock_path.file_name().unwrap().to_os_string();
    let mut s = name.clone();
    s.push(".lock");
    lock_path.set_file_name(s);
    std::fs::create_dir_all(&lock_path).unwrap();
    let _db = Database::open_path(&p).unwrap();
}

#[cfg(target_os = "linux")]
#[test]
fn reused_pid_lock_is_reclaimed_via_start_time() {
    // v7.34 (crash-recovery P0 #2) — the container pid-1 restart: the
    // dead owner was pid 1, the restarted process reuses pid 1, and
    // `ps -p 1` always succeeds — so a bare pid probe self-deadlocks.
    // The recorded start-time settles it: the lock carries the OLD
    // pid-1's start-time, which can't match the live pid's current
    // start-time, so the lock is stale and open reclaims it WITHOUT any
    // manual force_unlock (a SIGKILL'd mail server must self-recover).
    let p = tmpdb("reused-pid");
    let mut lock_path = p.clone();
    let name = lock_path.file_name().unwrap().to_os_string();
    let mut s = name.clone();
    s.push(".lock");
    lock_path.set_file_name(s);
    std::fs::create_dir_all(&lock_path).unwrap();
    // owner = our (live) pid, but a start-time that cannot match ours
    // (our real start-time is jiffies-since-boot, never "1"). Empty
    // host/boot keep the legacy same-host assumption.
    std::fs::write(
        lock_path.join("pid"),
        format!("{}\n\n\n1\n", std::process::id()),
    )
    .unwrap();
    // Reclaimed → clean open succeeds, no force_unlock.
    let _db = Database::open_path(&p).unwrap();
}

/// v7.37.10 (mailrs 2026-06-19 recurrence) — container PID-1 restart
/// MUST recover without intervention. The pre-fix behaviour refused
/// with "different host" because `docker compose up -d` recreates the
/// container with a new hostname, so the lock file's recorded hostname
/// never matched the prober's. Skip the host-identity check when the
/// recorded owner is PID 1; the start-time check below is more accurate
/// for the container case and correctly declares the prior generation
/// stale.
#[cfg(target_os = "linux")]
#[test]
fn container_pid1_lock_recovers_across_hostname_change() {
    let p = tmpdb("container-pid1-hostname");
    let mut lock_path = p.clone();
    let name = lock_path.file_name().unwrap().to_os_string();
    let mut s = name.clone();
    s.push(".lock");
    lock_path.set_file_name(s);
    std::fs::create_dir_all(&lock_path).unwrap();
    // Owner = pid 1 (containerised), recorded on a hostname that is
    // NOT this process's host, with a start-time that cannot match
    // pid 1's current /proc/1/stat (we use "1" — a sentinel from
    // boot jiffies that will never be the actual value at test time).
    std::fs::write(
        lock_path.join("pid"),
        "1\nold-container-7eb2c\nbootabc\n1\n",
    )
    .unwrap();
    // Reclaimed without force_unlock — clean open succeeds.
    let _db = Database::open_path(&p).unwrap();
}

/// v7.37.10 — pre-v7.34 lock format (no start-time line) on a PID-1
/// owner is unambiguously stale in containers, because the previous
/// container's PID 1 cannot share identity with the new container's
/// PID 1. Treat as stale and reclaim.
#[cfg(target_os = "linux")]
#[test]
fn legacy_pid1_lock_without_start_time_is_reclaimed() {
    let p = tmpdb("legacy-pid1-no-start");
    let mut lock_path = p.clone();
    let name = lock_path.file_name().unwrap().to_os_string();
    let mut s = name.clone();
    s.push(".lock");
    lock_path.set_file_name(s);
    std::fs::create_dir_all(&lock_path).unwrap();
    // Owner = pid 1, no host, no boot, no start-time — pre-v7.34
    // lock shape from an old container generation that died unclean.
    std::fs::write(lock_path.join("pid"), "1\n").unwrap();
    let _db = Database::open_path(&p).unwrap();
}

#[cfg(target_os = "linux")]
#[test]
fn live_self_lock_with_matching_start_time_is_held() {
    // The dual of the above: a lock owned by our pid WITH our real
    // start-time is a genuine live holder (e.g. an accidental
    // double-open of the same path in one process) and must be REFUSED,
    // not reclaimed — otherwise a second writer could steal a live lock.
    let p = tmpdb("self-held");
    let _db = Database::open_path(&p).unwrap(); // writes pid + real start-time
    match Database::open_path(&p) {
        Err(EngineError::Unsupported(msg)) => assert!(msg.contains("locked"), "got: {msg}"),
        other => panic!("expected lock error (live self-held), got {other:?}"),
    }
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
