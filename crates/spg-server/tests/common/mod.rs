//! Shared test helpers for spg-server integration tests.
//!
//! Each test binary that spawns an spg-server child includes this
//! module via `mod common;` at file-top. Because integration tests
//! compile to separate binaries, the module body is duplicated per
//! binary (the same way doctest examples are duplicated) — that's
//! the standard Cargo integration-test idiom.
//!
//! # Why this module exists
//!
//! Before v6.0.x every test file rolled its own `pick_free_addr` /
//! `wait_for_listener` pair:
//!
//! ```ignore
//! fn pick_free_addr() -> String {
//!     let p = TcpListener::bind("127.0.0.1:0").unwrap();
//!     let a = p.local_addr().unwrap();
//!     drop(p);            // ← TOCTOU window opens here
//!     a.to_string()       //   another test can grab the same port
//! }                       //   before the child's bind(addr) lands
//! ```
//!
//! Manifested as `Connection reset by peer` (one test's client
//! connected to another test's child) or `bind: Address in use`
//! (the second-to-spawn child died), depending on which race won.
//!
//! Fix: pass `127.0.0.1:0` straight to the spg-server child. The
//! kernel atomically allocates a port and holds it for the listener's
//! lifetime. The child prints `spg-server: listening on
//! 127.0.0.1:PORT` to stderr; this module tails until that line
//! appears, then returns the addr. No race, no probe-and-drop.
//!
//! For tests that need extra listeners (http / pg-wire / repl) we
//! pass `127.0.0.1:0` for each and parse the matching stderr line
//! shape (`http listening on …`, `pg-wire listening on …`,
//! `replication listening on …`).

#![allow(
    dead_code,
    clippy::doc_markdown,
    clippy::struct_excessive_bools,
    clippy::fn_params_excessive_bools
)]

use std::io::{BufRead, BufReader, Read};
use std::net::TcpStream;
use std::path::Path;
use std::process::{Child, ChildStderr, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

/// Default deadline for waiting on `listening on` / `connect`. Tests
/// that need a longer startup window construct the spawner directly
/// instead of going through the defaults.
pub const STARTUP_TIMEOUT: Duration = Duration::from_secs(10);

/// 7.38.1 S1.3 (D10) — the spawn deadline, env-tunable. Under a
/// loaded machine (parallel cargo from a neighbouring session) the
/// 10 s default produced 13 spurious "didn't publish listen addr"
/// reds in one gate run; the runner may widen it via
/// `SPG_TEST_SPAWN_DEADLINE_SECS` without touching code. A genuinely
/// dead server still fails — later and honestly.
pub fn startup_timeout() -> Duration {
    std::env::var("SPG_TEST_SPAWN_DEADLINE_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .map_or(STARTUP_TIMEOUT, Duration::from_secs)
}

/// v7.38.22 — how long this host takes to start a trivial process, right
/// now.
///
/// A spawn deadline that expires has two causes with identical symptoms:
/// the server is broken, or the machine cannot start processes. This
/// measures the second one directly, with a control that has nothing to
/// do with SPG — so the verdict rests on evidence rather than on whoever
/// reads the red remembering that the box was busy.
///
/// `None` when no control binary is available, and that matters: no
/// evidence means no forgiveness, so the caller fails exactly as it did
/// before rather than assuming the host was at fault.
pub fn spawn_control_latency() -> Option<Duration> {
    // v7.39.8 — the control is the BINARY UNDER TEST, not `/usr/bin/true`.
    //
    // `/usr/bin/true` is signed, tiny and long since validated by the
    // kernel; the server is ~13 MB, freshly linked and unsigned. On
    // macOS the FIRST execution of such a file pays a one-time
    // validation the second does not, and a control that never pays it
    // cannot see it. Measured on the development box, quiet:
    //
    // ```text
    //                              this box      the testbed
    //   a never-run copy, 1st run   212.0 ms        3.3 ms
    //   the same copy afterwards      3.1 ms        2.2 ms
    //   /usr/bin/true                 2.0 ms        1.4 ms
    // ```
    //
    // Under load the first run reached 9.1 s here — inside a 10 s
    // startup deadline, with several tests spawning at once. Thirteen
    // of them failed saying "the host starts processes promptly, so
    // this is the server", and the child was in fact still in `dyld`,
    // before `main`. The control was right about `/usr/bin/true` and
    // wrong about the question.
    //
    // `--replay-only` is the server's own immediate-exit path, so this
    // measures the same file through the same loader and returns.
    let dir = tmp_base().join(format!("spg-spawn-control-{}", std::process::id()));
    std::fs::create_dir_all(&dir).ok()?;
    let t = Instant::now();
    let status = Command::new(env!("CARGO_BIN_EXE_spg-server"))
        .arg("--replay-only")
        .arg(dir.join("a.db"))
        .arg(dir.join("a.wal"))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    let elapsed = t.elapsed();
    let _ = std::fs::remove_dir_all(&dir);
    status.ok()?;
    Some(elapsed)
}

/// Pay the binary's first-launch cost ONCE, before any deadline is
/// running.
///
/// This is the other half of the note on [`spawn_control_latency`], and
/// it is the half that stops the failures rather than explaining them.
/// The cost is per FILE, so every rebuild brings it back, and
/// `cargo test` rebuilds before it runs — which is why the first test
/// to spawn was the one that paid, inside its own startup deadline.
///
/// Costs 2-3 ms on a machine that does not do this at all.
fn warm_the_binary_under_test() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let dir = tmp_base().join(format!("spg-spawn-warm-{}", std::process::id()));
        if std::fs::create_dir_all(&dir).is_err() {
            return;
        }
        let _ = Command::new(env!("CARGO_BIN_EXE_spg-server"))
            .arg("--replay-only")
            .arg(dir.join("a.db"))
            .arg(dir.join("a.wal"))
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        let _ = std::fs::remove_dir_all(&dir);
    });
}

/// The control reading above which this host is not starting processes
/// promptly.
///
/// v7.39.8 — recalibrated for the control it now measures, which is the
/// server binary itself rather than `/usr/bin/true`. Measured warm:
/// 3.1 ms on the development box, 2.2 ms on the testbed. 20 ms is
/// several times the worst of those and two orders below what a host
/// doing first-launch validation produces — 212 ms quiet here, 9.1 s
/// under load. The old reading is kept because the number did not need
/// to change: `/usr/bin/true` starts in 0.9-2.1 ms across sixty samples
/// — far enough out that a working machine cannot reach it, and far below
/// what a machine deep in swap produces (the run that prompted this had
/// load 66-126 with 21.76 GB of 22.5 GB swap in use).
pub const SPAWN_CONTROL_STALL: Duration = Duration::from_millis(20);

/// Whether a missed spawn deadline is the HOST's doing.
///
/// Pure, so the rule can be pinned in both directions rather than
/// trusted. Three ways to answer no, and each matters:
///
///   * the child is gone — that is a server that died, and no amount of
///     machine load explains it;
///   * no control reading — see [`spawn_control_latency`];
///   * a prompt control — the machine started a process in under
///     [`SPAWN_CONTROL_STALL`] while the server could not publish a line
///     in seconds, which is the server's problem.
#[must_use]
pub fn host_stalled_the_spawn(child_alive: bool, control: Option<Duration>) -> bool {
    child_alive && control.is_some_and(|d| d >= SPAWN_CONTROL_STALL)
}

/// All listener addresses the spg-server child can publish on its
/// stderr. `native` is always present (it's the mandatory CLI arg);
/// the rest are populated only when the matching env opt-in is set.
#[derive(Debug, Clone)]
pub struct ServerAddrs {
    pub native: String,
    pub http: Option<String>,
    pub pgwire: Option<String>,
    pub mysqlwire: Option<String>,
    pub repl: Option<String>,
}

/// Builder for spg-server child invocations. Defaults: pass
/// `127.0.0.1:0` as the native addr (kernel-chosen port), pipe
/// stdout to `/dev/null`, pipe stderr so we can tail the
/// `listening on` line. The startup timeout, listener opt-ins,
/// env vars, and positional args are all customisable.
pub struct ServerBuilder {
    extra_args: Vec<String>,
    extra_env: Vec<(String, String)>,
    env_remove: Vec<String>,
    want_http: bool,
    want_pgwire: bool,
    want_mysqlwire: bool,
    want_repl: bool,
    startup_timeout: Duration,
    inherit_stderr_echo: bool,
}

impl Default for ServerBuilder {
    fn default() -> Self {
        Self {
            extra_args: Vec::new(),
            extra_env: Vec::new(),
            env_remove: alloc_default_env_remove(),
            want_http: false,
            want_pgwire: false,
            want_mysqlwire: false,
            want_repl: false,
            startup_timeout: startup_timeout(),
            inherit_stderr_echo: false,
        }
    }
}

/// The collation every server in this panel DECLARES, and the knob that
/// switches the panel to the other one.
///
/// v7.39.5 — it was declared nowhere, so it was the operator's shell:
/// `ServerBuilder` clears three variables and inherits the rest, and
/// both of this project's machines export `LANG=en_US.UTF-8`. The wire
/// panel has therefore been ordering text by a LOCALE, silently, while
/// the servers `proclib` starts declare `C` and every fixture in the
/// suite was authored under `C`. A panel whose configuration comes from
/// whoever typed the command is not a panel — a CI runner with `LANG`
/// unset was running a different one.
///
/// `C` is the default for the same reason `proclib` chose it. The knob
/// exists so the gate can run the whole panel a second time under the
/// collation the published image actually ships, the way the
/// sqllogictest corpus has run twice since v7.39.4.
///
/// A test that sets `SPG_LC_COLLATE` itself still wins: `extra_env` is
/// applied after this.
#[must_use]
pub fn panel_collation() -> String {
    std::env::var("SPG_E2E_DB_COLLATION")
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "C".to_string())
}

/// Whether this panel orders text by a locale rather than by bytes.
#[must_use]
pub fn panel_is_collated() -> bool {
    !panel_collation().eq_ignore_ascii_case("C")
}

/// Env vars cleared by every test by default — these would otherwise
/// leak from the caller's shell (a developer running `cargo test`
/// with `SPG_PASSWORD=foo bash -c …` would auth-fail every test).
fn alloc_default_env_remove() -> Vec<String> {
    vec![
        "SPG_PASSWORD".into(),
        "SPG_ADMIN_PASSWORD".into(),
        "SPG_PG_ADDR".into(),
    ]
}

impl ServerBuilder {
    /// New builder pre-pointed at the spg-server cargo bin.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Append a positional CLI arg (e.g. db_path, "-", wal_path).
    /// The native addr (`127.0.0.1:0`) is always the first arg —
    /// extras come after it.
    #[must_use]
    pub fn arg(mut self, a: impl Into<String>) -> Self {
        self.extra_args.push(a.into());
        self
    }

    /// Append a `Path` as a positional CLI arg.
    #[must_use]
    pub fn arg_path(mut self, p: &Path) -> Self {
        self.extra_args.push(p.to_string_lossy().into_owned());
        self
    }

    /// Set an env var (overrides any prior value).
    #[must_use]
    pub fn env(mut self, k: impl Into<String>, v: impl Into<String>) -> Self {
        self.extra_env.push((k.into(), v.into()));
        self
    }

    /// Suppress the default env-remove for one key. Rare; used by
    /// tests that explicitly want SPG_PASSWORD set.
    #[must_use]
    pub fn keep_env(mut self, k: &str) -> Self {
        self.env_remove.retain(|x| x != k);
        self
    }

    /// v7.38.22 — clear one more variable from the child's environment,
    /// on top of the three every spawn clears.
    ///
    /// The counterpart to [`Self::keep_env`], and it exists because a
    /// test that wants a server with NO store must not inherit one from
    /// whoever ran it.
    #[must_use]
    pub fn env_remove(mut self, k: &str) -> Self {
        if !self.env_remove.iter().any(|x| x == k) {
            self.env_remove.push(k.to_string());
        }
        self
    }

    /// Add an HTTP listener via `SPG_HTTP_ADDR=127.0.0.1:0`. The
    /// child's `http listening on …` stderr line will be parsed and
    /// the addr returned in `ServerAddrs::http`.
    #[must_use]
    pub fn with_http(mut self) -> Self {
        self.want_http = true;
        self.extra_env
            .push(("SPG_HTTP_ADDR".into(), "127.0.0.1:0".into()));
        self
    }

    /// Add a PG-wire listener via `SPG_PG_ADDR=127.0.0.1:0`.
    #[must_use]
    pub fn with_pgwire(mut self) -> Self {
        self.want_pgwire = true;
        self.env_remove.retain(|k| k != "SPG_PG_ADDR");
        self.extra_env
            .push(("SPG_PG_ADDR".into(), "127.0.0.1:0".into()));
        self
    }

    /// v7.17.0 Phase 3.P0-70 — add a MySQL-wire listener via
    /// `SPG_MYSQLWIRE_ADDR=127.0.0.1:0`.
    #[must_use]
    pub fn with_mysqlwire(mut self) -> Self {
        self.want_mysqlwire = true;
        self.env_remove.retain(|k| k != "SPG_MYSQLWIRE_ADDR");
        self.extra_env
            .push(("SPG_MYSQLWIRE_ADDR".into(), "127.0.0.1:0".into()));
        self
    }

    /// Add a replication listener via `SPG_REPL_ADDR=127.0.0.1:0`.
    #[must_use]
    pub fn with_repl(mut self) -> Self {
        self.want_repl = true;
        self.extra_env
            .push(("SPG_REPL_ADDR".into(), "127.0.0.1:0".into()));
        self
    }

    /// v6.1.8 — set `SPG_WAL_LEVEL=logical` so MAGIC_SUB
    /// subscribers can attach. Default in production is
    /// `replica`; tests that exercise the v6.1.4+ subscription
    /// surface call this to flip on the logical-replication
    /// gate at startup.
    #[must_use]
    pub fn with_logical_wal(mut self) -> Self {
        self.extra_env
            .push(("SPG_WAL_LEVEL".into(), "logical".into()));
        self
    }

    /// Echo every stderr line back to the test's stderr (useful when
    /// the test wants server logs visible on failure). Default is to
    /// drain stderr to a sink after the listen line(s) parse.
    #[must_use]
    pub fn echo_stderr(mut self, on: bool) -> Self {
        self.inherit_stderr_echo = on;
        self
    }

    /// Override the wait-for-listen-line deadline.
    #[must_use]
    pub fn startup_timeout(mut self, d: Duration) -> Self {
        self.startup_timeout = d;
        self
    }

    /// Spawn the child and tail stderr until every requested listener
    /// has published its addr. Returns `(child, addrs)`.
    ///
    /// Panics on:
    ///   - the child exiting before all addrs appear,
    ///   - the timeout elapsing first,
    ///   - stderr read error.
    pub fn spawn(self) -> (Child, ServerAddrs) {
        warm_the_binary_under_test();
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_spg-server"));
        cmd.arg("127.0.0.1:0");
        for a in &self.extra_args {
            cmd.arg(a);
        }
        cmd.stdout(Stdio::null()).stderr(Stdio::piped());
        for k in &self.env_remove {
            cmd.env_remove(k);
        }
        // Declared, not inherited — see `panel_collation`. Applied
        // before `extra_env` so a test that names its own wins.
        cmd.env("SPG_LC_COLLATE", panel_collation());
        for (k, v) in &self.extra_env {
            cmd.env(k, v);
        }
        let mut child = cmd.spawn().expect("spawn spg-server");
        let stderr = child.stderr.take().expect("piped stderr");
        let addrs = read_listener_addrs(
            &mut child,
            stderr,
            self.startup_timeout,
            self.want_http,
            self.want_pgwire,
            self.want_mysqlwire,
            self.want_repl,
            self.inherit_stderr_echo,
        );
        (child, addrs)
    }

    /// Spawn variant for tests that *expect the server to exit during
    /// startup* (e.g. replay-failure tests). Skips the listen-line
    /// reader so the caller can poll `try_wait` and assert the exit
    /// status. stderr/stdout both go to `/dev/null`.
    pub fn spawn_expecting_startup_failure(self) -> Child {
        warm_the_binary_under_test();
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_spg-server"));
        cmd.arg("127.0.0.1:0");
        for a in &self.extra_args {
            cmd.arg(a);
        }
        cmd.stdout(Stdio::null()).stderr(Stdio::null());
        for k in &self.env_remove {
            cmd.env_remove(k);
        }
        cmd.env("SPG_LC_COLLATE", panel_collation());
        for (k, v) in &self.extra_env {
            cmd.env(k, v);
        }
        cmd.spawn().expect("spawn spg-server")
    }
}

/// Tail the child's stderr, parsing every `listening on` line shape
/// the server publishes, until every requested listener has reported
/// its addr.
#[allow(clippy::too_many_arguments, clippy::fn_params_excessive_bools)]
fn read_listener_addrs(
    child: &mut Child,
    stderr: ChildStderr,
    deadline: Duration,
    want_http: bool,
    want_pgwire: bool,
    want_mysqlwire: bool,
    want_repl: bool,
    inherit_echo: bool,
) -> ServerAddrs {
    let mut reader = BufReader::new(stderr);
    let until = Instant::now() + deadline;
    let mut native: Option<String> = None;
    let mut http: Option<String> = None;
    let mut pgwire: Option<String> = None;
    let mut mysqlwire: Option<String> = None;
    let mut repl: Option<String> = None;
    let mut line = String::new();
    while Instant::now() < until {
        if native.is_some()
            && (!want_http || http.is_some())
            && (!want_pgwire || pgwire.is_some())
            && (!want_mysqlwire || mysqlwire.is_some())
            && (!want_repl || repl.is_some())
        {
            break;
        }
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => {
                if let Ok(Some(status)) = child.try_wait() {
                    panic!("server exited before publishing addrs: {status:?}");
                }
                thread::sleep(Duration::from_millis(20));
            }
            Ok(_) => {
                if inherit_echo {
                    eprint!("{line}");
                }
                if let Some(a) = extract("http listening on ", &line) {
                    http = Some(a);
                } else if let Some(a) = extract("pg-wire listening on ", &line) {
                    pgwire = Some(a);
                } else if let Some(a) = extract("mysql-wire listening on ", &line) {
                    mysqlwire = Some(a);
                } else if let Some(a) = extract("replication listening on ", &line) {
                    repl = Some(a);
                } else if let Some(a) = extract("listening on ", &line) {
                    native = Some(a);
                }
            }
            Err(e) => panic!("read stderr: {e}"),
        }
    }
    let Some(n) = native else {
        // v7.38.22 — say WHY, with a control that is not this server.
        let alive = !matches!(child.try_wait(), Ok(Some(_)));
        let control = spawn_control_latency();
        let _ = child.kill();
        let verdict = if host_stalled_the_spawn(alive, control) {
            "this host is not starting processes promptly, so the deadline is about the \
             machine and not about the server"
        } else if !alive {
            "the child is gone — the server exited during startup"
        } else {
            "the host starts processes promptly, so this is the server"
        };
        panic!(
            "server didn't publish native listen addr within {deadline:?} — {verdict}. \
             (child alive: {alive}; a trivial process took {control:?} to start, stall \
             threshold {SPAWN_CONTROL_STALL:?})"
        );
    };
    if want_http && http.is_none() {
        let _ = child.kill();
        panic!("server didn't publish http addr within {deadline:?}");
    }
    if want_pgwire && pgwire.is_none() {
        let _ = child.kill();
        panic!("server didn't publish pg-wire addr within {deadline:?}");
    }
    if want_mysqlwire && mysqlwire.is_none() {
        let _ = child.kill();
        panic!("server didn't publish mysql-wire addr within {deadline:?}");
    }
    if want_repl && repl.is_none() {
        let _ = child.kill();
        panic!("server didn't publish replication addr within {deadline:?}");
    }
    // Drain the rest of stderr so the pipe doesn't backpressure the
    // child. Echo lines if the test asked for it.
    thread::spawn(move || {
        if inherit_echo {
            let mut buf = String::new();
            while let Ok(n) = reader.read_line(&mut buf) {
                if n == 0 {
                    break;
                }
                eprint!("{buf}");
                buf.clear();
            }
        } else {
            let mut sink = String::new();
            let _ = reader.read_to_string(&mut sink);
        }
    });
    ServerAddrs {
        native: n,
        http,
        pgwire,
        mysqlwire,
        repl,
    }
}

fn extract(marker: &str, line: &str) -> Option<String> {
    let after = line.find(marker)?;
    let tail = &line[after + marker.len()..];
    let end = tail.find([' ', '\n', '\r']).unwrap_or(tail.len());
    Some(tail[..end].to_string())
}

/// Kill-on-drop wrapper. Tests bind the spawned `Child` to a
/// `ChildGuard` so the server is reaped even if a test panics.
pub struct ChildGuard(pub Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Process RSS in KiB via `ps -o rss= -p <pid>` (works on macOS +
/// Linux; portable across the platforms SPG tests run on). Returns
/// 0 on parse failure rather than panicking — the test owns the
/// failure assertion with a clearer message.
///
/// v6.0.1 step 8: promoted from `e2e_chaos_freeze.rs` so the SQ8
/// perf gate can share the helper.
pub fn rss_kib_of(pid: u32) -> u64 {
    let out = Command::new("ps")
        .arg("-o")
        .arg("rss=")
        .arg("-p")
        .arg(pid.to_string())
        .output();
    let Ok(out) = out else { return 0 };
    if !out.status.success() {
        return 0;
    }
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse::<u64>()
        .unwrap_or(0)
}

/// Connect to a known-bound server addr. Retries briefly because the
/// listener is up by the time stderr printed `listening on …`, but
/// the OS may need a tick to register the bind in the accept queue.
pub fn connect_to(addr: &str) -> TcpStream {
    let deadline = Instant::now() + startup_timeout();
    loop {
        match TcpStream::connect(addr) {
            Ok(s) => return s,
            Err(e) => {
                assert!(Instant::now() < deadline, "connect {addr}: {e}");
                thread::sleep(Duration::from_millis(10));
            }
        }
    }
}

/// v7.39 (round 784) — poll a condition instead of sleeping a fixed
/// proxy interval, for the tests whose intent is CONVERGENCE ("the
/// background worker eventually gets there").
///
/// Round 783 traced this session's parallel-load flakes to constants
/// that encode "about this long on an idle box"; two workspace suites
/// at once are enough to make the constant lie. Polling keeps the
/// assertion exactly as it was, returns as soon as the condition
/// holds (usually sooner than the sleep it replaces), and only spends
/// the deadline when the machine is genuinely slow.
///
/// NOT for tests whose intent is INVARIANCE ("must STAY at 1 row",
/// "cold_segment_count stays 0") — polling until such a predicate
/// holds returns immediately and weakens them. Round 784 found
/// several of those hiding among the sleep-then-assert sites.
pub fn wait_until(budget: std::time::Duration, mut cond: impl FnMut() -> bool) -> bool {
    let deadline = std::time::Instant::now() + budget;
    loop {
        if cond() {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

/// v7.38.19 — the base every test scratch directory hangs off.
///
/// 161 files across this workspace build a unique path under
/// `std::env::temp_dir().join("spg-tests")` per run and none of them removes it. On the
/// machine this was found on, `$TMPDIR` had reached **61,708 entries and
/// 30 GB** — and it was not only disk. `spg-server` swept that directory
/// at every start, so one `readdir` took **95 seconds** and every server
/// an e2e test spawned waited a minute and a half before it could
/// listen. The failures that produces read exactly like a busy machine:
/// `EWOULDBLOCK` on a socket read, "server didn't publish native listen
/// addr within Ns".
///
/// The server no longer scans `$TMPDIR` (its run files moved into
/// `spg-run/`), so what is left is the mess itself. Putting every test's
/// scratch under one directory does not stop the tests leaking — that
/// needs 161 edits with a guard type, and the common shape here is a
/// helper that CREATES the directory and returns a path INSIDE it, so a
/// drop guard would delete it before the caller could use it. What this
/// does do is make the mess removable in one `rm -rf`, and keep it out
/// of a directory the rest of the machine reads.
#[must_use]
pub fn tmp_base() -> std::path::PathBuf {
    let p = std::env::temp_dir().join("spg-tests");
    let _ = std::fs::create_dir_all(&p);
    p
}

#[cfg(test)]
mod spawn_verdict_tests {
    use super::{Duration, SPAWN_CONTROL_STALL, host_stalled_the_spawn};

    #[test]
    fn a_dead_child_is_never_the_host() {
        // The machine being on fire does not make a server that exited
        // into a server that is merely slow.
        assert!(!host_stalled_the_spawn(
            false,
            Some(SPAWN_CONTROL_STALL * 100)
        ));
    }

    #[test]
    fn no_control_reading_means_no_forgiveness() {
        assert!(!host_stalled_the_spawn(true, None));
    }

    #[test]
    fn a_prompt_control_points_at_the_server() {
        // Just under the threshold: 19 ms against 20, written out rather
        // than subtracted so the number the test means is the number it
        // says.
        assert!(!host_stalled_the_spawn(
            true,
            Some(Duration::from_millis(19))
        ));
    }

    #[test]
    fn a_stalled_control_points_at_the_host() {
        assert!(host_stalled_the_spawn(true, Some(SPAWN_CONTROL_STALL)));
        assert!(host_stalled_the_spawn(true, Some(SPAWN_CONTROL_STALL * 5)));
    }
}
