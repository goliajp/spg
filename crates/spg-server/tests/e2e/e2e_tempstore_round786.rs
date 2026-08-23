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
    let dir = crate::common::tmp_base().join(format!("spg-t35-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    // v7.38.19 — the run files live in `<dir>/spg-run/` now, so the
    // server's startup sweep reads only what SPG wrote rather than all of
    // `$TMPDIR`. This test counts the directory the runs are IN; counting
    // the parent would count the `spg-run` directory itself and read its
    // creation as a leaked file.
    let rd = crate::tempstore_shim::run_dir(&dir);
    let count = |p: &std::path::Path| std::fs::read_dir(p).map(Iterator::count).unwrap_or(0);
    let before = count(&rd);
    let mut run = crate::tempstore_shim::create_run_in(&dir).expect("factory");
    run.append(b"hello ").unwrap();
    run.append(b"world").unwrap();
    assert_eq!(run.bytes_written(), 11);
    // The file exists while the run is alive.
    assert_eq!(count(&rd), before + 1);

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
    assert_eq!(count(&rd), before);
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

/// r840 — is the spill's cost the file, or the way it is written?
///
/// Round 839 timed the sorter over memory-backed runs at 186 ms for
/// 400k rows and cleared the codec and the merge. What it excluded was
/// real file I/O, and the access pattern is the suspect: the sorter
/// appends a 4-byte length and then a ~208-byte body per row, and reads
/// them back the same way, against a `File` with no buffering on either
/// side. That is four syscalls a row — 1.6M for this volume.
///
/// `cargo test -p spg-server --release --test e2e r840 -- --ignored --nocapture`
#[test]
#[ignore]
fn r840_file_run_access_pattern_cost() {
    use std::time::Instant;
    const ROWS: usize = 400_000;
    let body = vec![b'y'; 208];

    let dir = crate::common::tmp_base().join(format!("spg-r840-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();

    let mut run = crate::tempstore_shim::create_run_in(&dir).expect("factory");
    let t0 = Instant::now();
    for _ in 0..ROWS {
        let len = u32::try_from(body.len()).unwrap();
        run.append(&len.to_le_bytes()).unwrap();
        run.append(&body).unwrap();
    }
    run.seal().unwrap();
    let write_phase = t0.elapsed();

    let t1 = Instant::now();
    let mut hdr = [0u8; 4];
    let mut buf = vec![0u8; 208];
    let mut got = 0usize;
    loop {
        let mut filled = 0;
        while filled < 4 {
            let n = run.read(&mut hdr[filled..]).unwrap();
            if n == 0 {
                break;
            }
            filled += n;
        }
        if filled == 0 {
            break;
        }
        let len = u32::from_le_bytes(hdr) as usize;
        let mut filled = 0;
        while filled < len {
            let n = run.read(&mut buf[filled..len]).unwrap();
            assert!(n > 0, "short read");
            filled += n;
        }
        got += 1;
    }
    let read_phase = t1.elapsed();

    eprintln!(
        "R840 rows={ROWS} bytes={} write={write_phase:?} read={read_phase:?} read_back={got}",
        (ROWS * 212) / (1024 * 1024)
    );
    assert_eq!(got, ROWS);
    let _ = std::fs::remove_dir_all(&dir);
}
