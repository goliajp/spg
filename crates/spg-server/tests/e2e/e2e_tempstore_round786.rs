//! Round 786 (T35 Phase A) — the spill seam: a host-provided run
//! round-trips, cleans itself up on drop, and its absence changes
//! nothing.
//!
//! Phase A installs storage without using it, so the load-bearing
//! assertion is the last one: with no factory the engine still refuses
//! an over-budget sort exactly as it did before.

use spg_engine::{Engine, TempRun};

#[test]
fn round786_run_round_trips_and_cleans_up() {
    // v7.39 (round 787) — a PRIVATE directory, passed explicitly. The
    // round-786 version counted entries in the shared temp dir and the
    // gate caught it flaking under the parallel suite (230276 vs
    // 230273): other processes churn /tmp while the test runs. Same
    // failure class this session spent two rounds characterising —
    // an observation that is only stable on a quiet machine.
    let dir = std::env::temp_dir().join(format!("spg-t35-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let before = std::fs::read_dir(&dir).unwrap().count();
    let mut run = crate::tempstore_shim::create_run_in(&dir).expect("factory");
    run.append(b"hello ").unwrap();
    run.append(b"world").unwrap();
    assert_eq!(run.bytes_written(), 11);
    // The file exists while the run is alive.
    assert_eq!(std::fs::read_dir(&dir).unwrap().count(), before + 1);

    run.seal().unwrap();
    let mut got = Vec::new();
    let mut buf = [0u8; 4];
    loop {
        let n = run.read(&mut buf).unwrap();
        if n == 0 {
            break;
        }
        got.extend_from_slice(&buf[..n]);
    }
    assert_eq!(got, b"hello world");

    drop(run);
    // Dropping removes the backing file — a cancelled query must not
    // leave scratch behind.
    assert_eq!(std::fs::read_dir(&dir).unwrap().count(), before);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn round786_without_a_factory_the_ceiling_is_unchanged() {
    // The Phase A promise: an engine with no host storage behaves as it
    // always has — over-budget materialisation refuses, it does not
    // silently succeed or silently spill.
    let e = Engine::new();
    assert!(!e.can_spill(), "a bare engine has no spill storage");
    let mut e = e.with_max_query_bytes(8 * 1024);
    e.execute("CREATE TABLE big (pad TEXT)").unwrap();
    e.execute("INSERT INTO big SELECT repeat('x', 200) FROM generate_series(1, 5000) g")
        .unwrap();
    let err = e
        .execute("SELECT pad FROM big ORDER BY pad")
        .expect_err("over-budget sort must still refuse without spill storage");
    assert!(
        format!("{err}").contains("max_query_bytes"),
        "expected the budget refusal, got {err}"
    );
}
