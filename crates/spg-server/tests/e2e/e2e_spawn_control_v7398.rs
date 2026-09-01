//! v7.39.8 — the spawn deadline's verdict, and the control it rests on.
//!
//! When a server does not publish its listen line in time the harness
//! says whose fault it is, and for a while it said the wrong thing.
//! The control it compared against was `/usr/bin/true`: signed, tiny,
//! long since validated by the kernel. The thing under test is ~13 MB,
//! freshly linked and unsigned, and on macOS the FIRST execution of
//! such a file pays a one-time validation the second does not.
//!
//! Measured, quiet, with a never-run copy of the same binary:
//!
//! ```text
//!                              this box      the testbed
//!   a never-run copy, 1st run   212.0 ms        3.3 ms
//!   the same copy afterwards      3.1 ms        2.2 ms
//!   /usr/bin/true                 2.0 ms        1.4 ms
//! ```
//!
//! And from inside the suite, on a binary nothing had run yet, the
//! control itself read 5.08 s.
//!
//! Under load the first run reached 9.1 s on this box — inside a 10 s
//! deadline, with several tests spawning at once. Thirteen of them
//! failed saying "the host starts processes promptly, so this is the
//! server", while the child was in fact still in `dyld`, before `main`.
//! The testbed does none of this, which is why the same suite was green
//! there and red here.
//!
//! Two changes followed: the control is now the binary under test, and
//! the first-launch cost is paid once before any deadline is running.
//!
//! What these pin is the RULE, which is pure and deterministic. The
//! warm-up itself is not pinned by a test, deliberately: on a quiet
//! machine the 212 ms it removes fits inside the deadline anyway, so
//! removing it flips nothing, and a pin that only reddens on a loaded
//! machine is a pin on the machine. The measurement above is its
//! evidence.

use crate::common::{SPAWN_CONTROL_STALL, host_stalled_the_spawn, spawn_control_latency};
use std::time::Duration;

#[test]
fn a_prompt_control_means_the_server_is_at_fault() {
    assert!(
        !host_stalled_the_spawn(true, Some(Duration::from_millis(3))),
        "3 ms is what this binary costs warm; a deadline missed beside it \
         is the server's"
    );
}

#[test]
fn a_stalling_control_means_the_host_is_at_fault() {
    // 212 ms is the measured first-launch cost on the development box.
    assert!(host_stalled_the_spawn(
        true,
        Some(Duration::from_millis(212))
    ));
    // And the loaded reading, which is the one that produced the wrong
    // verdict.
    assert!(host_stalled_the_spawn(true, Some(Duration::from_secs(9))));
}

#[test]
fn a_dead_child_is_never_the_hosts_fault() {
    assert!(
        !host_stalled_the_spawn(false, Some(Duration::from_secs(9))),
        "a server that exited is not explained by machine load"
    );
}

#[test]
fn no_reading_decides_nothing() {
    assert!(!host_stalled_the_spawn(true, None));
}

#[test]
fn the_threshold_sits_between_the_two_populations() {
    // Warm cost 2.2-3.1 ms measured across both machines; first-launch
    // 212 ms on the one that does it. The threshold has to separate
    // them with room on each side.
    assert!(
        SPAWN_CONTROL_STALL > Duration::from_millis(6),
        "too close to what a warm start costs"
    );
    assert!(
        SPAWN_CONTROL_STALL < Duration::from_millis(100),
        "too close to what a stalling host costs"
    );
}

#[test]
fn the_control_measures_the_binary_under_test() {
    // The first call may BE a first launch — writing this test taught
    // that the hard way: run on its own, with no `spawn` ahead of it to
    // warm the file, it read 5.08 s on this box. That is the cost the
    // warm-up exists to move out of the deadline, measured from inside
    // the suite.
    //
    // So warm, then read. The second reading is what the threshold is
    // calibrated against, and it has to be small on any host.
    let _ = spawn_control_latency();
    let d = spawn_control_latency().expect("the control must produce a reading");
    assert!(
        d < Duration::from_millis(500),
        "a warm start of the binary under test took {d:?}, far above the \
         2-3 ms it costs on either machine — the control is not \
         measuring what it claims"
    );
}
