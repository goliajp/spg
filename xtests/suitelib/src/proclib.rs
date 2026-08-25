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

/// One spawn attempt's failure: a cross-process port-claim race is
/// retryable with fresh ports; everything else is not.
enum SpawnAttempt {
    Fatal(String),
    PortRace(String),
}

impl SpawnAttempt {
    fn fatal(e: String) -> Self {
        Self::Fatal(e)
    }
}

/// All processes this run owns. Dropping the roster reaps.
#[derive(Default)]
pub struct Roster {
    procs: Vec<Proc>,
}

/// v7.38.19 — free means "nothing is serving it", which a bind test
/// alone does not establish.
///
/// Rust's `TcpListener::bind` sets `SO_REUSEADDR`, and on macOS and the
/// BSDs that permits binding `127.0.0.1:P` while another process holds
/// `0.0.0.0:P` — which is how every server here binds. So the probe
/// bound successfully and called an occupied port free.
///
/// It cost the locale-collation panel, whose second server comes from a
/// FRESH roster, so the "already ours" check above could not help
/// either: both legs landed on 25476, the panel's `SPG_URI` pointed at
/// the leg it was meant to be compared AGAINST, and it measured one
/// server against itself for the whole run. That is the same defect the
/// panel exists to catch, one version earlier, in the other direction.
///
/// A connect settles it. If anything answers, someone is serving.
fn port_is_free(p: u16) -> bool {
    if std::net::TcpListener::bind(("127.0.0.1", p)).is_err() {
        return false;
    }
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], p));
    std::net::TcpStream::connect_timeout(&addr, Duration::from_millis(150)).is_err()
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
        self.free_port_excluding(None)
    }

    /// v7.38.19 — one implementation, because there were two.
    ///
    /// The native listener's port was chosen by a SECOND copy of this
    /// loop, carrying the bare `TcpListener::bind` that the fix above
    /// replaced. So a server could be told to serve its native protocol
    /// on a port another server was already answering pgwire on, and a
    /// client asking that port for a Postgres connection got
    ///
    /// ```text
    /// received invalid response to SSL negotiation: -
    /// ```
    ///
    /// which reached a prerelease gate as "the locale panel failed",
    /// because the panel's own error was the only one anything printed.
    ///
    /// `held` is the port this same spawn has already claimed but not
    /// yet bound; nothing else knows about it, so it has to be passed.
    pub fn free_port_excluding(&self, held: Option<u16>) -> Result<u16, String> {
        // Rotate the scan start by pid so concurrent test PROCESSES
        // spread across the range instead of all courting the first
        // port — full runs saw three suites claim 25460 at once.
        let len = PORT_RANGE.end - PORT_RANGE.start;
        let off = (std::process::id() as u16) % len;
        for i in 0..len {
            let p = PORT_RANGE.start + (off + i) % len;
            if Some(p) == held || self.procs.iter().any(|x| x.port == p) {
                continue;
            }
            if !port_is_free(p) {
                continue;
            }
            return Ok(p);
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
        // 7.38.1 (S2.2 rider) — Gatekeeper warm-up: the FIRST exec of a
        // freshly built binary on macOS goes through an XProtect scan
        // that can take seconds on a busy box — every chronic
        // "spawned but printed nothing for 10-40s" flake this train
        // and the 7.38.0 release hit had a rebuild right before it. A
        // fast failing exec (bad addr) pays the scan once, outside any
        // deadline.
        warm_binary_once(binary);
        // Port-claim race (7.38.1 CP1): two test PROCESSES can probe
        // the same port as free, spawn, and the loser exits at bind
        // with the pgwire side already answering — the client then
        // sees "peer closed" mid-flight. The probe can't reserve, so
        // the spawn retries with fresh ports when the child dies young.
        let mut last_err = String::new();
        for _attempt in 0..3 {
            match self.spawn_server_attempt(name, binary, data_dir, timeout, pg_bind, envs) {
                Ok(port) => return Ok(port),
                Err(SpawnAttempt::Fatal(e)) => return Err(e),
                Err(SpawnAttempt::PortRace(e)) => {
                    println!("proclib: {name}: {e}; retrying with fresh ports");
                    last_err = e;
                    std::thread::sleep(Duration::from_millis(
                        50 + u64::from(std::process::id() % 7) * 30,
                    ));
                }
            }
        }
        Err(format!("{name}: {last_err} (after 3 port-race retries)"))
    }

    fn spawn_server_attempt(
        &mut self,
        name: &str,
        binary: &Path,
        data_dir: &Path,
        timeout: Duration,
        pg_bind: &str,
        envs: &[(&str, &str)],
    ) -> Result<u16, SpawnAttempt> {
        let fatal = SpawnAttempt::fatal;
        let pg_port = self.free_port().map_err(SpawnAttempt::Fatal)?;
        // The native listener's port comes from the SAME probe as the
        // pgwire one, excluding it. It used to come from a second copy
        // of that loop — see `free_port_excluding`.
        let native_port = self
            .free_port_excluding(Some(pg_port))
            .map_err(SpawnAttempt::Fatal)?;
        std::fs::create_dir_all(data_dir)
            .map_err(|e| fatal(format!("{name}: mkdir data dir: {e}")))?;
        let log = data_dir.join("server.log");
        let logf = std::fs::File::create(&log).map_err(|e| fatal(format!("{name}: log: {e}")))?;
        let child = Command::new(binary)
            .arg(format!("127.0.0.1:{native_port}"))
            .arg(data_dir.join("db"))
            .arg(data_dir.join("audit"))
            .arg(data_dir.join("wal"))
            .env("SPG_PG_ADDR", format!("{pg_bind}:{pg_port}"))
            // v7.38.19 — declare the collation instead of inheriting the
            // machine's, and do it HERE so no spawn site can forget.
            //
            // The testbed exports `LANG=en_US.UTF-8` and `LC_ALL`, so
            // every server the suite started inherited a locale
            // collation while every comment about the suite says `C`.
            // The sweep's baseline leg was the visible casualty -- the
            // locale panel added in this same version was comparing
            // en_US against en_US and reporting no losses -- but five
            // more servers had the same exposure, including the one that
            // opens a released data directory and the pair that runs the
            // dump round-trip.
            //
            // `C` is what every fixture and expectation in this suite was
            // authored under. A caller that wants otherwise passes
            // `SPG_LC_COLLATE` in `envs`, which is applied after this and
            // therefore wins.
            .env("SPG_LC_COLLATE", "C")
            .envs(envs.iter().map(|(k, v)| (*k, *v)))
            .stdout(Stdio::from(
                logf.try_clone()
                    .map_err(|e| fatal(format!("{name}: log: {e}")))?,
            ))
            .stderr(Stdio::from(logf))
            .spawn()
            .map_err(|e| fatal(format!("{name}: spawn {}: {e}", binary.display())))?;
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
        let mut answered = false;
        while Instant::now() < deadline {
            // A young death is the port race's signature: the loser of
            // a cross-process bind claim exits(1) — even after its
            // pgwire side briefly answered. Confirm the child is still
            // alive AFTER the port answers before handing it out.
            let died = matches!(
                self.procs.last_mut().and_then(|p| p.child.try_wait().ok()),
                Some(Some(_))
            );
            if died {
                let p = self.procs.pop().expect("just spawned");
                let _ = std::fs::remove_dir_all(&p.data_dir);
                return Err(SpawnAttempt::PortRace(format!(
                    "exited during startup on port {pg_port} (bind race)"
                )));
            }
            if answered {
                return Ok(pg_port);
            }
            if TcpStream::connect(("127.0.0.1", pg_port)).is_ok() {
                answered = true;
                // Grace beat: a bind-race loser answers on pgwire,
                // then exits when the native bind fails — give it
                // time to die visibly before trusting the port.
                std::thread::sleep(Duration::from_millis(100));
                continue;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let mut tail = String::new();
        if let Ok(mut f) = std::fs::File::open(&log) {
            let _ = f.read_to_string(&mut tail);
        }
        let tail: String = tail.lines().rev().take(5).collect::<Vec<_>>().join(" | ");
        Err(fatal(format!(
            "{name}: port {pg_port} not answering after {timeout:?}; log tail: {tail}"
        )))
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
    /// v7.38.14 — stop and SAY SO rather than walk forever.
    ///
    /// This is a size report for one server's data directory, which holds
    /// tens of files. Pointed at a directory that is not one -- a test once
    /// handed it `/tmp` itself -- it walked every build artefact on the
    /// machine: 60 % CPU for over seven minutes, and a release train stuck
    /// behind it with nothing in the log to say why.
    ///
    /// A bound turns that into a number and a line of output. It is
    /// deliberately far above any real data directory, so a breach means the
    /// caller passed the wrong path, not that a server grew.
    const MAX_ENTRIES: u32 = 50_000;
    fn walk(d: &Path, acc: &mut u64, seen: &mut u32) {
        if *seen >= MAX_ENTRIES {
            return;
        }
        if let Ok(rd) = std::fs::read_dir(d) {
            for e in rd.filter_map(Result::ok) {
                *seen += 1;
                if *seen >= MAX_ENTRIES {
                    println!(
                        "proclib: dir_size_kb gave up after {MAX_ENTRIES} entries under {} \
                         — that is not a data directory; the size below is a floor",
                        d.display()
                    );
                    return;
                }
                let p = e.path();
                if p.is_dir() {
                    walk(&p, acc, seen);
                } else if let Ok(m) = e.metadata() {
                    *acc += m.len();
                }
            }
        }
    }
    let mut bytes = 0u64;
    let mut seen = 0u32;
    walk(dir, &mut bytes, &mut seen);
    bytes / 1024
}

/// One fast throwaway exec per binary path per process — see the
/// Gatekeeper note at the call site.
fn warm_binary_once(binary: &Path) {
    use std::collections::HashSet;
    use std::sync::Mutex;
    static WARMED: Mutex<Option<HashSet<PathBuf>>> = Mutex::new(None);
    let mut g = WARMED
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let set = g.get_or_insert_with(HashSet::new);
    if !set.insert(binary.to_path_buf()) {
        return;
    }
    drop(g);
    let _ = Command::new(binary)
        .arg("warmup-invalid-addr")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
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
/// v7.38.19 — under the same root every other test scratch uses.
///
/// This hard-coded `/tmp/spg-suite-<runid>`, which is neither `$TMPDIR`
/// nor the `spg-tests` root this release moved 161 leaking test files
/// into — so the suite's own run directories were exactly the thing the
/// scratch-root gate was written to stop, and the gate could not see
/// them because it scans for `env::temp_dir()` and this does not call
/// it.
///
/// `/tmp` is kept rather than `$TMPDIR` because a suite run spawns
/// servers whose socket and data paths appear in process listings and
/// error messages, and `/tmp/spg-tests/spg-suite-<runid>` stays legible
/// where a `/var/folders/rx/1_v1…/T/` prefix does not. What changes is
/// that one `rm -rf /tmp/spg-tests` now collects the suite's leavings
/// along with everything else's.
pub fn run_tmp_dir(runid: &str) -> PathBuf {
    let base = PathBuf::from("/tmp/spg-tests");
    let _ = std::fs::create_dir_all(&base);
    base.join(format!("spg-suite-{runid}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The server-spawning fault tests all probe the same suite port
    /// range; run in parallel they race the probe (TOCTOU) and pile
    /// onto one port — full run 1 saw all three claim 25460 at once.
    /// A shared lock beats runner discipline (--test-threads=1 was a
    /// doc note, and the full tier didn't read it).
    ///
    /// 7.38.1 CP — this MUST be the same mutex every server-spawning
    /// test module locks. A second static in this module let these
    /// tests interleave with `wireclient_split_tests` (which locks
    /// `tests_support::guard`): both fulls of 2026-08-19 went red on
    /// one box each with the same signature — a freshly spawned server
    /// with an empty data dir while the client talked to a sibling
    /// test's server that had won the port race.
    fn server_test_guard() -> std::sync::MutexGuard<'static, ()> {
        super::tests_support::guard()
    }

    #[test]
    fn free_port_is_inside_the_suite_range() {
        let r = Roster::new();
        let p = r.free_port().expect("a free port");
        assert!(PORT_RANGE.contains(&p), "{p}");
    }

    /// v7.38.19 — a port ANOTHER process is serving on the wildcard
    /// address must not be handed out.
    ///
    /// Rust's `TcpListener::bind` sets `SO_REUSEADDR`, and on macOS and
    /// the BSDs that permits binding `127.0.0.1:P` while something else
    /// holds `0.0.0.0:P`. The probe therefore called an occupied port
    /// free.
    ///
    /// It cost the locale-collation panel, which spawns its second
    /// server from a FRESH roster — so the "already ours" check could
    /// not help either. Both legs landed on 25476, the panel's
    /// `SPG_URI` pointed at the leg it was supposed to be compared
    /// AGAINST, and it spent the run measuring one server against
    /// itself. Which is the same defect that panel was added to catch,
    /// one version earlier, in the other direction.
    ///
    /// It surfaced only because this version also made the panel state
    /// which collation it expects. Without that it would have gone on
    /// reporting `losses=0` for a comparison it was not making.
    #[test]
    fn free_port_skips_a_port_another_process_is_serving() {
        let _serial = server_test_guard();
        let r = Roster::new();
        let victim = r.free_port().expect("a free port");
        // Hold it the way a spawned server does: the wildcard address.
        let held = std::net::TcpListener::bind(("0.0.0.0", victim))
            .expect("the probe just said this port was free");
        for _ in 0..8 {
            let p = r.free_port().expect("a free port");
            assert_ne!(
                p, victim,
                "handed out a port that another listener is serving"
            );
        }
        drop(held);
    }

    /// 7.38.1 CP1 — a child that dies during startup (the bind-race
    /// signature) is retried with fresh ports, and the final error
    /// names the retries instead of hanging out the full timeout.
    #[test]
    fn early_exit_spawn_retries_then_reports_the_race() {
        let _serial = server_test_guard();
        let tmp = run_tmp_dir("port-race");
        let _ = std::fs::remove_dir_all(&tmp);
        let mut r = Roster::new();
        let err = r
            .spawn_server(
                "racer",
                Path::new("/usr/bin/false"),
                &tmp,
                Duration::from_secs(10),
            )
            .expect_err("a startup death must not hand out the port");
        assert!(
            err.contains("port-race retries"),
            "want the retry trail in the error, got: {err}"
        );
        assert!(r.procs.is_empty(), "dead attempts must not linger");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn reap_all_reports_and_clears() {
        let mut r = Roster::new();
        // A child that would outlive the test if not reaped.
        let child = Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("spawn sleep");
        // v7.38.14 — an EMPTY directory of our own, not `temp_dir()` itself.
        //
        // `reap_all_checked` calls `dir_size_kb(&p.data_dir)`, which walks the
        // whole tree recursively. Handing it `/tmp` made this "spawn a sleep
        // and reap it" test walk every build artefact every project on the
        // machine had left there: measured on a developer box whose /tmp held
        // an 848 MB tarball and several 250 MB+ trees, the test span at 60 %
        // CPU for over seven minutes and `find /tmp -type f` did not finish
        // counting in twenty seconds. The same code passed in 100 s on a
        // testbed whose /tmp was clean.
        //
        // So the test's verdict depended on the machine's /tmp rather than on
        // the code, which is the property a test must not have. It blocked a
        // release train to say so.
        let data_dir = std::env::temp_dir()
            .join("spg-tests")
            .join(alloc_probe_dir_name());
        std::fs::create_dir_all(&data_dir).expect("probe dir");
        r.procs.push(Proc {
            name: "sleeper".into(),
            child,
            port: PORT_RANGE.start,
            peak_rss_kb: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
            data_dir: data_dir.clone(),
        });
        r.reap_all();
        assert!(r.procs.is_empty());
        let _ = std::fs::remove_dir_all(&data_dir);
    }

    /// A directory name no other run will pick: pid plus a monotonic counter.
    fn alloc_probe_dir_name() -> String {
        use std::sync::atomic::{AtomicU32, Ordering};
        static N: AtomicU32 = AtomicU32::new(0);
        format!(
            "spg-proclib-probe-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        )
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
        let mut spawned_pids: Vec<u32> = Vec::new();
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
            spawned_pids.extend(r.procs.iter().map(|p| p.child.id()));
            r.reap_all();
            // The port must be re-bindable immediately after reaping.
            // (SO_REUSEADDR semantics make the bind probe pass even in
            // TIME_WAIT; what matters is no living process owns it.)
        }
        // 7.38.1 CP note — assert on the pids THIS test spawned, not a
        // global pgrep: the old pattern-match caught any unrelated
        // spg-server on the box (a dev server, a concurrent suite) and
        // failed the run for someone else's process.
        for pid in &spawned_pids {
            let alive = Command::new("kill")
                .args(["-0", &pid.to_string()])
                .status()
                .map(|st| st.success())
                .unwrap_or(false);
            assert!(!alive, "leaked server pid {pid}");
        }
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

#[cfg(test)]
mod second_port_tests {
    use super::*;

    /// v7.38.19 — the native listener's port must come from the same
    /// probe as the pgwire one.
    ///
    /// It came from a second copy of the loop, carrying the bare bind
    /// that `port_is_free` replaced, so a server could be told to serve
    /// its native protocol on a port another server was already
    /// answering pgwire on. A client then got `received invalid response
    /// to SSL negotiation`, and a prerelease gate reported it as the
    /// locale panel disagreeing about a collation.
    #[test]
    fn the_second_port_also_skips_a_port_someone_is_serving() {
        let r = Roster::new();
        // v7.38.20 — take the port, do not merely be told it is free.
        //
        // Between the probe and the bind, another test in the same run
        // can take it, and this failed a release commit that way: the
        // bind's `expect` fired with "the probe just said this port was
        // free", which was true when it said it. The test is about the
        // EXCLUSION, so it retries until it actually holds one.
        let (first, held) = (0..64)
            .find_map(|_| {
                let p = r.free_port().ok()?;
                std::net::TcpListener::bind(("0.0.0.0", p))
                    .ok()
                    .map(|l| (p, l))
            })
            .expect("a port this test can hold");
        for _ in 0..8 {
            let second = r.free_port_excluding(Some(9)).expect("a free port");
            assert_ne!(second, first, "handed out a port someone is serving");
        }
        drop(held);
    }

    /// And it excludes what the same spawn has already claimed but not
    /// yet bound — which nothing else can know about.
    #[test]
    fn the_second_port_is_never_the_first() {
        let r = Roster::new();
        let first = r.free_port().expect("a free port");
        for _ in 0..8 {
            assert_ne!(
                r.free_port_excluding(Some(first)).expect("a free port"),
                first
            );
        }
    }
}
