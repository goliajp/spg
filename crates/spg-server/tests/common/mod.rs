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

/// All listener addresses the spg-server child can publish on its
/// stderr. `native` is always present (it's the mandatory CLI arg);
/// the rest are populated only when the matching env opt-in is set.
#[derive(Debug, Clone)]
pub struct ServerAddrs {
    pub native: String,
    pub http: Option<String>,
    pub pgwire: Option<String>,
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
            want_repl: false,
            startup_timeout: STARTUP_TIMEOUT,
            inherit_stderr_echo: false,
        }
    }
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

    /// Add a replication listener via `SPG_REPL_ADDR=127.0.0.1:0`.
    #[must_use]
    pub fn with_repl(mut self) -> Self {
        self.want_repl = true;
        self.extra_env
            .push(("SPG_REPL_ADDR".into(), "127.0.0.1:0".into()));
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
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_spg-server"));
        cmd.arg("127.0.0.1:0");
        for a in &self.extra_args {
            cmd.arg(a);
        }
        cmd.stdout(Stdio::null()).stderr(Stdio::piped());
        for k in &self.env_remove {
            cmd.env_remove(k);
        }
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
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_spg-server"));
        cmd.arg("127.0.0.1:0");
        for a in &self.extra_args {
            cmd.arg(a);
        }
        cmd.stdout(Stdio::null()).stderr(Stdio::null());
        for k in &self.env_remove {
            cmd.env_remove(k);
        }
        for (k, v) in &self.extra_env {
            cmd.env(k, v);
        }
        cmd.spawn().expect("spawn spg-server")
    }
}

/// Tail the child's stderr, parsing every `listening on` line shape
/// the server publishes, until every requested listener has reported
/// its addr.
fn read_listener_addrs(
    child: &mut Child,
    stderr: ChildStderr,
    deadline: Duration,
    want_http: bool,
    want_pgwire: bool,
    want_repl: bool,
    inherit_echo: bool,
) -> ServerAddrs {
    let mut reader = BufReader::new(stderr);
    let until = Instant::now() + deadline;
    let mut native: Option<String> = None;
    let mut http: Option<String> = None;
    let mut pgwire: Option<String> = None;
    let mut repl: Option<String> = None;
    let mut line = String::new();
    while Instant::now() < until {
        if native.is_some()
            && (!want_http || http.is_some())
            && (!want_pgwire || pgwire.is_some())
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
        let _ = child.kill();
        panic!("server didn't publish native listen addr within {deadline:?}");
    };
    if want_http && http.is_none() {
        let _ = child.kill();
        panic!("server didn't publish http addr within {deadline:?}");
    }
    if want_pgwire && pgwire.is_none() {
        let _ = child.kill();
        panic!("server didn't publish pg-wire addr within {deadline:?}");
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

/// Connect to a known-bound server addr. Retries briefly because the
/// listener is up by the time stderr printed `listening on …`, but
/// the OS may need a tick to register the bind in the accept queue.
pub fn connect_to(addr: &str) -> TcpStream {
    let deadline = Instant::now() + STARTUP_TIMEOUT;
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
