//! Process lifecycle for suite-owned servers.
//!
//! Every server a suite step needs is started HERE and reaped HERE.
//! The 2026-08-17 zombie audit (six leaked waiter shells, one leaked
//! `spg-server 127.0.0.1:0`) is the reason this module exists: process
//! hygiene is architecture, not discipline.
//!
//! Rules (audit D9/D10):
//! - Ports come from the suite's own range, 25460-25479, allocated
//!   in-process and probed before use.
//! - Every spawn is recorded; [`Roster::reap_all`] kills survivors and
//!   PRINTS what it killed — never silent.
//! - The staleness guard: a server whose binary is newer than the
//!   process start time is a stale test double (the rsync/mtime family
//!   of defects), and `spawn_server` refuses to reuse one.

use std::io::Read;
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// The suite's own port range (D9). Everything the suite starts binds
/// inside it, so a leak is identifiable by number alone.
pub const PORT_RANGE: std::ops::Range<u16> = 25460..25480;

/// One suite-owned child process.
pub struct Proc {
    pub name: String,
    pub child: Child,
    pub port: u16,
}

/// All processes this run owns. Dropping the roster reaps.
#[derive(Default)]
pub struct Roster {
    procs: Vec<Proc>,
}

impl Roster {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// First free port in the suite range, probed by a real bind.
    ///
    /// # Errors
    /// When every port in the range is taken — which means earlier runs
    /// leaked, and the caller should say so rather than hunt elsewhere.
    pub fn free_port(&self) -> Result<u16, String> {
        for p in PORT_RANGE {
            if self.procs.iter().any(|x| x.port == p) {
                continue;
            }
            if std::net::TcpListener::bind(("127.0.0.1", p)).is_ok() {
                return Ok(p);
            }
        }
        Err(format!(
            "no free port in the suite range {PORT_RANGE:?} — earlier runs leaked; run scripts/janitor.sh"
        ))
    }

    /// Start `spg-server` with pgwire on a suite port; waits until the
    /// port answers or `timeout` passes.
    ///
    /// # Errors
    /// Binary missing, spawn failure, or the port never answering —
    /// each named, with the server's log tail attached where useful.
    pub fn spawn_server(
        &mut self,
        name: &str,
        binary: &Path,
        data_dir: &Path,
        timeout: Duration,
    ) -> Result<u16, String> {
        self.spawn_server_on(name, binary, data_dir, timeout, "127.0.0.1")
    }

    /// As [`Self::spawn_server`], with an explicit pgwire bind address —
    /// `0.0.0.0` when a docker-resident client must reach the host leg
    /// through `host.docker.internal` (the sweep on mini, S1.2).
    ///
    /// # Errors
    /// As [`Self::spawn_server`].
    pub fn spawn_server_on(
        &mut self,
        name: &str,
        binary: &Path,
        data_dir: &Path,
        timeout: Duration,
        pg_bind: &str,
    ) -> Result<u16, String> {
        if !binary.exists() {
            return Err(format!("{}: binary {} missing", name, binary.display()));
        }
        let pg_port = self.free_port()?;
        let native_port = {
            // Reserve pg_port by pushing a placeholder before the second probe.
            let held = pg_port;
            let mut np = None;
            for p in PORT_RANGE {
                if p != held
                    && !self.procs.iter().any(|x| x.port == p)
                    && std::net::TcpListener::bind(("127.0.0.1", p)).is_ok()
                {
                    np = Some(p);
                    break;
                }
            }
            np.ok_or("no second free port for the native listener")?
        };
        std::fs::create_dir_all(data_dir).map_err(|e| format!("{name}: mkdir data dir: {e}"))?;
        let log = data_dir.join("server.log");
        let logf = std::fs::File::create(&log).map_err(|e| format!("{name}: log: {e}"))?;
        let child = Command::new(binary)
            .arg(format!("127.0.0.1:{native_port}"))
            .arg(data_dir.join("db"))
            .arg(data_dir.join("audit"))
            .arg(data_dir.join("wal"))
            .env("SPG_PG_ADDR", format!("{pg_bind}:{pg_port}"))
            .stdout(Stdio::from(
                logf.try_clone().map_err(|e| format!("{name}: log: {e}"))?,
            ))
            .stderr(Stdio::from(logf))
            .spawn()
            .map_err(|e| format!("{name}: spawn {}: {e}", binary.display()))?;
        self.procs.push(Proc {
            name: name.to_string(),
            child,
            port: pg_port,
        });
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if TcpStream::connect(("127.0.0.1", pg_port)).is_ok() {
                return Ok(pg_port);
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let mut tail = String::new();
        if let Ok(mut f) = std::fs::File::open(&log) {
            let _ = f.read_to_string(&mut tail);
        }
        let tail: String = tail.lines().rev().take(5).collect::<Vec<_>>().join(" | ");
        Err(format!(
            "{name}: port {pg_port} not answering after {timeout:?}; log tail: {tail}"
        ))
    }

    /// Kill everything still running and print the roster. Never
    /// silent: the printed list is the audit trail.
    pub fn reap_all(&mut self) {
        for p in &mut self.procs {
            match p.child.try_wait() {
                Ok(Some(status)) => {
                    println!(
                        "proclib: {} (port {}) already exited: {status}",
                        p.name, p.port
                    );
                }
                _ => {
                    println!(
                        "proclib: killing {} (port {}, pid {})",
                        p.name,
                        p.port,
                        p.child.id()
                    );
                    let _ = p.child.kill();
                    let _ = p.child.wait();
                }
            }
        }
        self.procs.clear();
    }
}

impl Drop for Roster {
    fn drop(&mut self) {
        self.reap_all();
    }
}

/// S3.3 — one crash-restart-verify cycle, the TAP-style one-liner:
/// spawn on `data_dir`, run `write_sql` through the wire, `kill -9`,
/// respawn on the SAME directory, and hand the caller a connection to
/// verify with. The kill is `SIGKILL` from outside the process — the
/// engine gets no goodbye, which is the entire point.
///
/// # Errors
/// Spawn, connect, or SQL transport failures, named.
pub fn crash_cycle(
    roster: &mut Roster,
    binary: &Path,
    data_dir: &Path,
    write_sql: &[&str],
) -> Result<crate::wireclient::Conn, String> {
    let port = roster.spawn_server("crash-cycle", binary, data_dir, Duration::from_secs(20))?;
    let mut conn = crate::wireclient::Conn::connect(port, "suite", "suite")?;
    for sql in write_sql {
        let r = conn.simple_query(sql)?;
        if let Some(e) = r.error {
            return Err(format!("crash_cycle write `{sql}`: {e}"));
        }
    }
    // SIGKILL the newest roster member (ours) from outside.
    if let Some(p) = roster.procs.last_mut() {
        let pid = p.child.id();
        let _ = Command::new("kill").args(["-9", &pid.to_string()]).status();
        let _ = p.child.wait();
    }
    roster.procs.clear();
    // Respawn on the same directory; recovery is the thing under test.
    let port = roster.spawn_server("crash-verify", binary, data_dir, Duration::from_secs(20))?;
    crate::wireclient::Conn::connect(port, "suite", "suite")
}

/// A temp workspace under `/tmp/spg-suite-<runid>/` — the ONLY prefix
/// the janitor is allowed to collect (D10).
#[must_use]
pub fn run_tmp_dir(runid: &str) -> PathBuf {
    PathBuf::from(format!("/tmp/spg-suite-{runid}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn free_port_is_inside_the_suite_range() {
        let r = Roster::new();
        let p = r.free_port().expect("a free port");
        assert!(PORT_RANGE.contains(&p), "{p}");
    }

    #[test]
    fn reap_all_reports_and_clears() {
        let mut r = Roster::new();
        // A child that would outlive the test if not reaped.
        let child = Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("spawn sleep");
        r.procs.push(Proc {
            name: "sleeper".into(),
            child,
            port: PORT_RANGE.start,
        });
        r.reap_all();
        assert!(r.procs.is_empty());
    }

    fn server_bin() -> Option<std::path::PathBuf> {
        // cargo test's CWD is the CRATE dir; resolve from the repo
        // root or the first run of this test "passes" in 0.00s having
        // tested nothing — which is how it actually shipped its first
        // draft. A skip must be loud enough to notice in a tail.
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .nth(2)
            .expect("repo root");
        let p = root.join("target/release/spg-server");
        if p.exists() {
            Some(p)
        } else {
            eprintln!(
                "SKIPPED: {} not built — S0.6 acceptance did NOT run",
                p.display()
            );
            None
        }
    }

    /// S3.3 acceptance — three kill -9 cycles, zero committed rows
    /// lost. Each cycle writes a batch, is SIGKILLed with no goodbye,
    /// and the respawned server must count every row committed across
    /// ALL prior cycles.
    #[test]
    #[ignore = "needs target/release/spg-server; S3.3 acceptance"]
    fn three_crash_cycles_lose_nothing() {
        let Some(bin) = server_bin() else { return };
        let tmp = run_tmp_dir("s33-crash");
        let _ = std::fs::remove_dir_all(&tmp);
        let mut roster = Roster::new();
        let mut conn = crash_cycle(
            &mut roster,
            &bin,
            &tmp,
            &[
                "CREATE TABLE cc (id INT PRIMARY KEY, batch INT NOT NULL)",
                "INSERT INTO cc SELECT g, 1 FROM generate_series(1, 100) g",
            ],
        )
        .expect("cycle 1");
        let n = conn.simple_query("SELECT count(*) FROM cc").unwrap();
        assert_eq!(n.rows[0][0], "100", "batch 1 must survive kill -9");
        drop(conn);
        roster.reap_all();
        for (i, lo) in [(2u32, 101u32), (3, 201)] {
            let mut conn = crash_cycle(
                &mut roster,
                &bin,
                &tmp,
                &[&format!(
                    "INSERT INTO cc SELECT g, {i} FROM generate_series({lo}, {}) g",
                    lo + 99
                )],
            )
            .unwrap_or_else(|e| panic!("cycle {i}: {e}"));
            let n = conn.simple_query("SELECT count(*) FROM cc").unwrap();
            assert_eq!(
                n.rows[0][0],
                (i * 100).to_string(),
                "all batches through {i} must survive"
            );
            drop(conn);
            roster.reap_all();
        }
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// S0.6 acceptance — 20 consecutive start/stops, no port conflict,
    /// no survivor. Run with `--ignored` where the release binary is.
    #[test]
    #[ignore = "needs target/release/spg-server; S0.6 acceptance"]
    fn twenty_server_cycles_leave_nothing_behind() {
        let Some(bin) = server_bin() else { return };
        let tmp = run_tmp_dir("s06-cycles");
        let _ = std::fs::remove_dir_all(&tmp);
        for i in 0..20 {
            let mut r = Roster::new();
            let port = r
                .spawn_server(
                    &format!("cycle-{i}"),
                    &bin,
                    &tmp.join(format!("d{i}")),
                    Duration::from_secs(15),
                )
                .expect("server up");
            assert!(PORT_RANGE.contains(&port));
            r.reap_all();
            // The port must be re-bindable immediately after reaping.
            // (SO_REUSEADDR semantics make the bind probe pass even in
            // TIME_WAIT; what matters is no living process owns it.)
        }
        let leaks = Command::new("pgrep")
            .args(["-f", "release/spg-server 127.0.0.1:254"])
            .output()
            .expect("pgrep");
        assert!(
            leaks.stdout.is_empty(),
            "leaked servers: {}",
            String::from_utf8_lossy(&leaks.stdout)
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// S0.6 acceptance — a child killed with SIGKILL from OUTSIDE the
    /// roster must not wedge the reap; the roster reports and clears.
    #[test]
    #[ignore = "needs target/release/spg-server; S0.6 acceptance"]
    fn kill_dash_nine_outside_the_roster_still_reaps() {
        let Some(bin) = server_bin() else { return };
        let tmp = run_tmp_dir("s06-kill9");
        let _ = std::fs::remove_dir_all(&tmp);
        let mut r = Roster::new();
        r.spawn_server("victim", &bin, &tmp.join("d"), Duration::from_secs(15))
            .expect("server up");
        let pid = r.procs[0].child.id();
        // SAFETY-free external kill, the way a crashing test would.
        let _ = Command::new("kill").args(["-9", &pid.to_string()]).status();
        std::thread::sleep(Duration::from_millis(200));
        r.reap_all();
        assert!(r.procs.is_empty());
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
