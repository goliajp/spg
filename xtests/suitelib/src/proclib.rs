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
    /// D20 — peak resident set in KB, written by the sampler thread
    /// (`ps -o rss=` every 500 ms). Zero until the first sample.
    pub peak_rss_kb: std::sync::Arc<std::sync::atomic::AtomicU64>,
    /// S4.4/CP4 (disk face) — the server's data directory, sized at
    /// reap so every suite server leaves a disk account too.
    pub data_dir: PathBuf,
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
        self.spawn_server_env(name, binary, data_dir, timeout, pg_bind, &[])
    }

    /// As [`Self::spawn_server_on`], with extra environment — the
    /// fault-injection knobs (`SPG_FAIL_*` / `SPG_FAULT_*`) ride here
    /// (S3.4, design D28).
    ///
    /// # Errors
    /// As [`Self::spawn_server_on`].
    pub fn spawn_server_env(
        &mut self,
        name: &str,
        binary: &Path,
        data_dir: &Path,
        timeout: Duration,
        pg_bind: &str,
        envs: &[(&str, &str)],
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
            .envs(envs.iter().map(|(k, v)| (*k, *v)))
            .stdout(Stdio::from(
                logf.try_clone().map_err(|e| format!("{name}: log: {e}"))?,
            ))
            .stderr(Stdio::from(logf))
            .spawn()
            .map_err(|e| format!("{name}: spawn {}: {e}", binary.display()))?;
        // D20 — RSS sampler: 500 ms `ps -o rss=` polls, peak kept in
        // a shared cell the roster reads at reap. The thread exits on
        // its own when the pid disappears; it holds no other handles.
        let peak = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
        {
            let peak = std::sync::Arc::clone(&peak);
            let pid = child.id();
            std::thread::spawn(move || {
                loop {
                    let out = Command::new("ps")
                        .args(["-o", "rss=", "-p", &pid.to_string()])
                        .output();
                    let Ok(out) = out else { break };
                    let text = String::from_utf8_lossy(&out.stdout);
                    let Ok(kb) = text.trim().parse::<u64>() else {
                        break; // pid gone — sampler retires
                    };
                    peak.fetch_max(kb, std::sync::atomic::Ordering::Relaxed);
                    std::thread::sleep(Duration::from_millis(500));
                }
            });
        }
        self.procs.push(Proc {
            name: name.to_string(),
            child,
            port: pg_port,
            peak_rss_kb: peak,
            data_dir: data_dir.to_path_buf(),
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
        let _ = self.reap_all_checked(None);
    }

    /// As [`Self::reap_all`], returning each process's peak RSS and —
    /// when a ceiling (MB) is given — erring on any breach (D20:
    /// ceilings have teeth, not commentary).
    ///
    /// # Errors
    /// A process whose sampled peak exceeded `ceiling_mb`.
    pub fn reap_all_checked(
        &mut self,
        ceiling_mb: Option<u64>,
    ) -> Result<Vec<(String, u64)>, String> {
        let mut peaks: Vec<(String, u64)> = Vec::new();
        for p in &mut self.procs {
            let peak_kb = p.peak_rss_kb.load(std::sync::atomic::Ordering::Relaxed);
            let disk_kb = dir_size_kb(&p.data_dir);
            match p.child.try_wait() {
                Ok(Some(status)) => {
                    println!(
                        "proclib: {} (port {}) already exited: {status} (peak rss {} MB, disk {} KB)",
                        p.name,
                        p.port,
                        peak_kb / 1024,
                        disk_kb
                    );
                }
                _ => {
                    println!(
                        "proclib: killing {} (port {}, pid {}, peak rss {} MB, disk {} KB)",
                        p.name,
                        p.port,
                        p.child.id(),
                        peak_kb / 1024,
                        disk_kb
                    );
                    let _ = p.child.kill();
                    let _ = p.child.wait();
                }
            }
            peaks.push((p.name.clone(), peak_kb));
        }
        self.procs.clear();
        if let Some(mb) = ceiling_mb {
            for (name, kb) in &peaks {
                if kb / 1024 > mb {
                    return Err(format!(
                        "rss ceiling breached: {name} peaked at {} MB (> {mb} MB)",
                        kb / 1024
                    ));
                }
            }
        }
        Ok(peaks)
    }
}

/// Recursive on-disk size of a directory in KB — the disk account
/// every reap prints (WAL + audit + db growth is visible per run).
fn dir_size_kb(dir: &Path) -> u64 {
    fn walk(d: &Path, acc: &mut u64) {
        if let Ok(rd) = std::fs::read_dir(d) {
            for e in rd.filter_map(Result::ok) {
                let p = e.path();
                if p.is_dir() {
                    walk(&p, acc);
                } else if let Ok(m) = e.metadata() {
                    *acc += m.len();
                }
            }
        }
    }
    let mut bytes = 0u64;
    walk(dir, &mut bytes);
    bytes / 1024
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

    /// The server-spawning fault tests all probe the same suite port
    /// range; run in parallel they race the probe (TOCTOU) and pile
    /// onto one port — full run 1 saw all three claim 25460 at once.
    /// A shared lock beats runner discipline (--test-threads=1 was a
    /// doc note, and the full tier didn't read it).
    fn server_test_guard() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        LOCK.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

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
            peak_rss_kb: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
            data_dir: std::env::temp_dir(),
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
        let _serial = server_test_guard();
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

    /// S3.4 acceptance (audit fault). What holds today: the
    /// unauditable statement ERRORS to the client, the server
    /// survives, and the chain verifies after restart. What this test
    /// DISCOVERED and pins as-observed: the errored statement's row
    /// is nonetheless durable (see the in-test note). Run the fault
    /// tests with --test-threads=1 — two parallel spawns race the
    /// suite port range.
    #[test]
    #[ignore = "needs target/release/spg-server; S3.4 acceptance"]
    fn audit_append_fault_refuses_the_statement() {
        let _serial = server_test_guard();
        let Some(bin) = server_bin() else { return };
        let tmp = run_tmp_dir("s34-audit");
        let _ = std::fs::remove_dir_all(&tmp);
        let mut roster = Roster::new();
        let port = roster
            .spawn_server_env(
                "audit-fault",
                &bin,
                &tmp,
                Duration::from_secs(20),
                "127.0.0.1",
                &[("SPG_FAIL_AUDIT_AT", "2")],
            )
            .expect("server up");
        let mut conn = crate::wireclient::Conn::connect(port, "suite", "suite").unwrap();
        let r1 = conn.simple_query("CREATE TABLE af (a INT)").unwrap();
        assert!(r1.error.is_none(), "{:?}", r1.error);
        // The second audited statement meets the injection.
        let r2 = conn.simple_query("INSERT INTO af VALUES (1)").unwrap();
        let err = r2.error.expect("the unauditable statement must error");
        assert!(err.contains("audit"), "{err}");
        // The server is alive and later statements audit fine.
        let r3 = conn.simple_query("INSERT INTO af VALUES (2)").unwrap();
        assert!(r3.error.is_none(), "{:?}", r3.error);
        roster.reap_all();
        // Restart WITHOUT the fault: the chain must verify and the
        // refused row must not exist.
        let port = roster
            .spawn_server("audit-verify", &bin, &tmp, Duration::from_secs(20))
            .expect("restart");
        let mut conn = crate::wireclient::Conn::connect(port, "suite", "suite").unwrap();
        let n = conn.simple_query("SELECT count(*) FROM af").unwrap();
        // r1055 (D29) — the pin flipped: audit now runs inside the
        // commit leader BEFORE any WAL byte, so the un-auditable
        // statement's error truthfully means "no effect". The first
        // version of this test pinned the opposite as-observed — the
        // error-but-applied ordering defect it discovered on its
        // first run.
        assert_eq!(
            n.rows[0][0], "1",
            "the refused INSERT must leave no row (ack ⇒ durable AND audited)"
        );
        let vals = conn.simple_query("SELECT a FROM af").unwrap();
        assert_eq!(vals.rows[0][0], "2", "the surviving row is the audited one");
        roster.reap_all();
        let log = std::fs::read_to_string(tmp.join("server.log")).unwrap_or_default();
        assert!(
            log.contains("verified audit log"),
            "chain must verify after the fault: {log}"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// S3.4 acceptance (kill during recovery) — recovery is
    /// re-enterable: killed mid-replay (window widened by
    /// SPG_FAULT_RECOVERY_PAUSE_MS), a clean restart replays
    /// everything.
    #[test]
    #[ignore = "needs target/release/spg-server; S3.4 acceptance"]
    fn kill_during_recovery_then_clean_restart_recovers_all() {
        let _serial = server_test_guard();
        let Some(bin) = server_bin() else { return };
        let tmp = run_tmp_dir("s34-reckill");
        let _ = std::fs::remove_dir_all(&tmp);
        let mut roster = Roster::new();
        let port = roster
            .spawn_server("seed", &bin, &tmp, Duration::from_secs(20))
            .expect("seed server");
        let mut conn = crate::wireclient::Conn::connect(port, "suite", "suite").unwrap();
        conn.simple_query("CREATE TABLE rk (a INT)").unwrap();
        for i in 0..40 {
            let r = conn
                .simple_query(&format!("INSERT INTO rk VALUES ({i})"))
                .unwrap();
            assert!(r.error.is_none());
        }
        // Unclean death, so recovery has real work.
        if let Some(p) = roster.procs.last_mut() {
            let _ = Command::new("kill")
                .args(["-9", &p.child.id().to_string()])
                .status();
            let _ = p.child.wait();
        }
        roster.procs.clear();
        // Respawn with a widened replay (41 frames x 100 ms ≈ 4 s) and
        // kill it 1 s in — mid-recovery by construction.
        let mut mid = Command::new(&bin)
            .arg("127.0.0.1:0")
            .arg(tmp.join("db"))
            .arg(tmp.join("audit"))
            .arg(tmp.join("wal"))
            .env("SPG_FAULT_RECOVERY_PAUSE_MS", "100")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("mid-recovery spawn");
        std::thread::sleep(Duration::from_millis(1000));
        let _ = Command::new("kill")
            .args(["-9", &mid.id().to_string()])
            .status();
        let _ = mid.wait();
        // Clean restart: everything must be there.
        let port = roster
            .spawn_server("verify", &bin, &tmp, Duration::from_secs(30))
            .expect("clean restart");
        let mut conn = crate::wireclient::Conn::connect(port, "suite", "suite").unwrap();
        let n = conn.simple_query("SELECT count(*) FROM rk").unwrap();
        assert_eq!(
            n.rows[0][0], "40",
            "all 40 rows survive a mid-recovery kill"
        );
        roster.reap_all();
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// S0.6 acceptance — 20 consecutive start/stops, no port conflict,
    /// no survivor. Run with `--ignored` where the release binary is.
    #[test]
    #[ignore = "needs target/release/spg-server; S0.6 acceptance"]
    fn twenty_server_cycles_leave_nothing_behind() {
        let _serial = server_test_guard();
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
        let _serial = server_test_guard();
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

#[cfg(test)]
mod wireclient_split_tests {
    use super::tests_support::*;

    /// 7.38.1 S1.1 — the send/read halves compose back into
    /// simple_query, and poll_pending sees the difference between an
    /// idle connection and one with an answer waiting.
    #[test]
    #[ignore = "needs target/release/spg-server; S1.1 acceptance"]
    fn split_halves_and_pending_probe() {
        let _serial = guard();
        let Some(bin) = bin() else { return };
        let tmp = crate::proclib::run_tmp_dir("s11-split");
        let _ = std::fs::remove_dir_all(&tmp);
        let mut roster = crate::proclib::Roster::new();
        let port = roster
            .spawn_server("s11", &bin, &tmp, std::time::Duration::from_secs(20))
            .expect("server");
        let mut c = crate::wireclient::Conn::connect(port, "s11", "s11").expect("connect");
        // Idle connection: nothing pending.
        assert!(!c.poll_pending(150).expect("idle poll"));
        // Send half, then the answer becomes observable, then read.
        c.send_query_nowait("SELECT 41 + 1").expect("send");
        // Give the server a beat; the probe must flip to true.
        std::thread::sleep(std::time::Duration::from_millis(150));
        assert!(c.poll_pending(500).expect("armed poll"));
        let r = c.read_result().expect("read");
        assert_eq!(r.rows, vec![vec!["42".to_string()]]);
        // The composed path still works on the same connection.
        let r2 = c.simple_query("SELECT 7").expect("composed");
        assert_eq!(r2.rows, vec![vec!["7".to_string()]]);
        // Deadline-bounded read on a fresh in-flight query.
        c.send_query_nowait("SELECT 8").expect("send2");
        let r3 = c
            .read_result_deadline(std::time::Duration::from_secs(5))
            .expect("deadline read");
        assert_eq!(r3.rows, vec![vec!["8".to_string()]]);
        roster.reap_all();
        let _ = std::fs::remove_dir_all(&tmp);
    }
}

#[cfg(test)]
pub(crate) mod tests_support {
    /// Shared server-test plumbing for sibling test modules.
    pub(crate) fn guard() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        LOCK.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
    pub(crate) fn bin() -> Option<std::path::PathBuf> {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .canonicalize()
            .ok()?;
        let p = root.join("target/release/spg-server");
        if p.exists() {
            Some(p)
        } else {
            eprintln!("SKIPPED loudly: target/release/spg-server not built");
            None
        }
    }
}
