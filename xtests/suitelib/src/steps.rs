//! Internal step implementations for `suite-run` (S0.10).
//!
//! Each returns Ok(summary) on pass, Err(reason) on fail — the ledger
//! does the timing, this module does the work.

use crate::crategraph::CrateGraph;
use crate::proclib::Roster;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

fn sh(root: &Path, cmd: &str) -> Result<String, String> {
    let out = Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .current_dir(root)
        .output()
        .map_err(|e| format!("spawn `{cmd}`: {e}"))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        Err(format!(
            "`{cmd}` exited {}:\n{}{}",
            out.status,
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        ))
    }
}

/// Workspace crates touched by the uncommitted diff (vs HEAD), mapped
/// through the crate graph's directory table.
///
/// # Errors
/// git failures only; an empty diff is Ok(empty).
pub fn changed_crates(root: &Path, graph: &CrateGraph) -> Result<Vec<String>, String> {
    let diff = sh(root, "git diff --name-only HEAD")?;
    let mut hits: Vec<String> = Vec::new();
    for file in diff.lines() {
        for (name, dir) in &graph.dirs {
            if file.starts_with(dir.as_str()) && !hits.contains(name) {
                hits.push(name.clone());
            }
        }
    }
    hits.sort();
    Ok(hits)
}

/// `clippy-affected` — clippy over the affected closure, debug profile.
///
/// # Errors
/// Clippy findings (the output is the reason).
pub fn clippy_affected(root: &Path, graph: &CrateGraph) -> Result<String, String> {
    let changed = changed_crates(root, graph)?;
    if changed.is_empty() {
        return Ok("no crate changes — skipped".into());
    }
    let affected = graph.affected(&changed);
    let flags: String = affected.iter().map(|c| format!(" -p {c}")).collect();
    sh(root, &format!("cargo clippy -q{flags} -- -D warnings"))?;
    Ok(format!("clippy clean over {} crates", affected.len()))
}

/// `unit-affected` — `--lib --bins` tests over the affected closure.
///
/// # Errors
/// Test failures (the output is the reason).
pub fn unit_affected(root: &Path, graph: &CrateGraph) -> Result<String, String> {
    let changed = changed_crates(root, graph)?;
    if changed.is_empty() {
        return Ok("no crate changes — skipped".into());
    }
    let affected = graph.affected(&changed);
    let flags: String = affected.iter().map(|c| format!(" -p {c}")).collect();
    // v7.38.2 — cargo errors on `--lib` when a SINGLE selected package
    // has no lib target (bins-only spg-server / spgctl), but silently
    // tolerates the same flags across MULTIPLE packages — so this step
    // only ever failed when exactly one bins-only crate changed. Retry
    // without `--lib` on that precise error.
    match sh(root, &format!("cargo test -q{flags} --lib --bins")) {
        Ok(_) => {}
        Err(e) if e.contains("no library targets") => {
            sh(root, &format!("cargo test -q{flags} --bins"))?;
        }
        Err(e) => return Err(e),
    }
    Ok(format!("unit green over {} crates", affected.len()))
}

/// `perf-sweep` (S1.2) — the release-blocking endpoint sweep with its
/// legs configured HERE, never from operator env: PERF blocked two
/// release trains for missing URIs before this step existed.
///
/// Environment detection is by artifact, not hostname: the mini
/// testbed carries `~/spgbench/bin/psql` (a docker-exec wrapper whose
/// container reaches the host as `host.docker.internal`); anywhere
/// else uses local psql and 127.0.0.1 for both legs. Both configs keep
/// the two legs on ONE host string — r1022's symmetry rule.
///
/// # Errors
/// Missing PG leg, server failure, or `losses>0` — the verdict line is
/// quoted either way.
pub fn perf_sweep(root: &Path, runid: &str) -> Result<String, String> {
    let bin = root.join("target/release/spg-server");
    if !bin.exists() {
        sh(root, "cargo build --release -q -p spg-server")?;
    }
    let home = std::env::var("HOME").map_err(|_| "no $HOME")?;
    let wrapper = Path::new(&home).join("spgbench/bin/psql");
    let (psql, host, bind) = if wrapper.exists() {
        (
            wrapper.display().to_string(),
            "host.docker.internal",
            "0.0.0.0",
        )
    } else {
        ("psql".to_string(), "127.0.0.1", "127.0.0.1")
    };
    let pg_uri = format!("postgres://bench:bench@{host}:25432/bench");
    let tmp = crate::proclib::run_tmp_dir(&format!("{runid}-sweep"));
    let _ = std::fs::remove_dir_all(&tmp);
    let mut roster = Roster::new();
    let port = roster.spawn_server_on("sweep-leg", &bin, &tmp, Duration::from_secs(20), bind)?;
    let spg_uri = format!("postgres://bench:bench@{host}:{port}/bench");
    // Both legs must answer before anything is timed (r1041).
    for uri in [&pg_uri, &spg_uri] {
        sh(
            root,
            &format!("{psql} --no-psqlrc -X -q -tA '{uri}' -c 'SELECT 1'"),
        )
        .map_err(|e| format!("leg {uri} not answering: {e}"))?;
    }
    let out = sh(
        root,
        &format!(
            "PSQL='{psql}' PG_URI='{pg_uri}' SPG_URI='{spg_uri}' bash scripts/perf-endpoint-sweep.sh"
        ),
    );
    // D20 — the sweep leg's peak RSS goes into the account, and the
    // manifest ceiling has teeth at reap.
    let ceiling = std::fs::read_to_string(root.join("xtests/suite.toml"))
        .ok()
        .and_then(|t| crate::config::Manifest::parse(&t).ok())
        .map(|m| m.meta.rss_ceiling_mb)
        .filter(|&mb| mb > 0);
    let peaks = roster.reap_all_checked(ceiling)?;
    let peak_note: Vec<String> = peaks
        .iter()
        .map(|(n, kb)| format!("{n}={} MB", kb / 1024))
        .collect();
    let _ = std::fs::remove_dir_all(&tmp);
    let text = out?;
    let verdict = text
        .lines()
        .rev()
        .find(|l| l.contains("losses="))
        .unwrap_or("(no verdict line)")
        .to_string();
    if verdict.contains("losses=0") {
        Ok(format!("{verdict}; peak rss: {}", peak_note.join(", ")))
    } else {
        Err(format!("sweep verdict: {verdict}"))
    }
}

/// `ironrules` (S1.3) — the prerelease tier's standing-rule step: the
/// wire smoke below, PLUS the previous release's data directory opened
/// directly by the CURRENT binary and verified row-for-row.
///
/// The fixture (`xtests/compat-datadirs/v7.38.7/`) was captured by
/// the v7.38.7 tag's own binary: 500 rows across nine types, two
/// indexes, deletes and updates, statement-level WAL (there is no db
/// file; replay IS the open). `expected.txt` holds counts and an md5
/// over an ordered projection, so a silently thinner replay cannot
/// pass. Older fixtures stay on disk beside it (S3.2).
///
/// # Errors
/// Any probe or any fixture assertion failing, named.
pub fn ironrules_full(root: &Path, runid: &str) -> Result<String, String> {
    let smoke = ironrule_smoke(root, runid)?;
    let fixture = root.join("xtests/compat-datadirs/v7.38.7");
    if !fixture.join("expected.txt").exists() {
        return Err(format!("fixture missing: {}", fixture.display()));
    }
    let tmp = crate::proclib::run_tmp_dir(&format!("{runid}-fver"));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).map_err(|e| format!("mkdir: {e}"))?;
    // The server mutates its dir; always open a COPY.
    for f in ["audit", "wal", "wal.cluster_id"] {
        std::fs::copy(fixture.join(f), tmp.join(f)).map_err(|e| format!("copy {f}: {e}"))?;
    }
    let bin = root.join("target/release/spg-server");
    let mut roster = Roster::new();
    let port = roster.spawn_server("fver", &bin, &tmp, Duration::from_secs(15))?;
    let mut conn = crate::wireclient::Conn::connect(port, "suite", "suite")?;
    let expected = std::fs::read_to_string(fixture.join("expected.txt"))
        .map_err(|e| format!("expected.txt: {e}"))?;
    for line in expected.lines() {
        let Some((key, want)) = line.trim().split_once(' ') else {
            continue;
        };
        let sql = if key == "checksum" {
            "SELECT md5(string_agg(t, ',' ORDER BY id)) FROM fx_scalars".to_string()
        } else {
            format!("SELECT count(*) FROM {key}")
        };
        let r = conn.simple_query(&sql)?;
        if let Some(e) = r.error {
            return Err(format!("v7.38.7 fixture: {sql}: {e}"));
        }
        let got = r
            .rows
            .first()
            .and_then(|row| row.first())
            .cloned()
            .unwrap_or_default();
        if got != want {
            return Err(format!(
                "v7.38.7 fixture: {key}: want {want}, got {got} — the previous \
                 release's data did not survive the current binary"
            ));
        }
    }
    roster.reap_all();
    let _ = std::fs::remove_dir_all(&tmp);
    Ok(format!("{smoke}; v7.38.7 dir direct-open verified"))
}

/// `ironrule-smoke` — the fastest wire-level pins of standing rules:
///
/// 1. `wal_path` is really plumbed (r964's hard lesson): after one
///    write, the WAL file at OUR path is non-empty.
/// 2. The pgwire listener answers psql's first packet (SSLRequest).
/// 3. A zero-column result set still carries its rows (the r800 gate
///    relaxation lost them once): `SELECT FROM t` returns one DataRow
///    per row, zero fields each.
///
/// # Errors
/// Any probe failing, with the probe named.
pub fn ironrule_smoke(root: &Path, runid: &str) -> Result<String, String> {
    let bin = root.join("target/release/spg-server");
    if !bin.exists() {
        return Err(format!(
            "{} not built — precommit needs the release server once; run `cargo build --release -p spg-server`",
            bin.display()
        ));
    }
    let tmp = crate::proclib::run_tmp_dir(&format!("{runid}-ironrule"));
    let _ = std::fs::remove_dir_all(&tmp);
    let mut roster = Roster::new();
    let port = roster.spawn_server("ironrule", &bin, &tmp, Duration::from_secs(15))?;

    // Probe 2 first — it needs no state.
    let answer = crate::wireclient::ssl_request_answered(port)?;
    if answer != b'S' && answer != b'N' {
        return Err(format!(
            "SSLRequest answered {answer:#x}, expected 'S' or 'N'"
        ));
    }

    let mut conn = crate::wireclient::Conn::connect(port, "suite", "suite")?;
    let run = |c: &mut crate::wireclient::Conn,
               sql: &str|
     -> Result<crate::wireclient::QueryResult, String> {
        let r = c.simple_query(sql)?;
        match &r.error {
            Some(e) => Err(format!("{sql}: {e}")),
            None => Ok(r),
        }
    };
    run(&mut conn, "CREATE TABLE irt (a INT)")?;
    run(&mut conn, "INSERT INTO irt VALUES (1), (2)")?;

    // Probe 3 — zero-column rows.
    let zc = run(&mut conn, "SELECT FROM irt")?;
    if zc.rows.len() != 2 || zc.n_columns != 0 {
        return Err(format!(
            "zero-column SELECT: want 2 rows x 0 cols, got {} rows x {} cols",
            zc.rows.len(),
            zc.n_columns
        ));
    }

    // Probe 1 — the WAL at OUR path grew past its header.
    let wal = tmp.join("wal");
    let wal_len = std::fs::metadata(&wal).map(|m| m.len()).unwrap_or(0);
    if wal_len == 0 {
        return Err(format!(
            "wal_path not plumbed: {} is empty after two writes",
            wal.display()
        ));
    }

    roster.reap_all();
    let _ = std::fs::remove_dir_all(&tmp);
    Ok(format!(
        "ssl answered '{}', wal {} bytes, zero-col rows intact",
        answer as char, wal_len
    ))
}

/// full-tier `perm-matrix` — the whole permutation matrix, whole
/// corpus (no --fast, no sampling). Builds the release server first:
/// the two wire permutations refuse to guess at a stale binary.
///
/// # Errors
/// Build failure or any permutation reporting failures.
pub fn perm_matrix(root: &Path) -> Result<String, String> {
    sh(
        root,
        "cargo build -q --release -p spg-server -p spg-perm-runner",
    )?;
    sh(root, "cargo run -q --release -p spg-perm-runner -- all").map(|out| tail_lines(&out, 10))
}

/// full-tier `oracle-three` — bring up the D13-pinned compose stack,
/// run all three differential legs, and ALWAYS tear the stack down
/// (zombie discipline): the teardown runs whether the legs pass or
/// not, and a teardown failure surfaces even on a green run.
///
/// # Errors
/// Stack startup, any leg's unexplained diff, or teardown failure.
pub fn oracle_three(root: &Path) -> Result<String, String> {
    // OrbStack keeps docker off the default PATH on the runners.
    let orb = "/Applications/OrbStack.app/Contents/MacOS/xbin";
    let path = std::env::var("PATH").unwrap_or_default();
    if !path.split(':').any(|p| p == orb) && std::path::Path::new(orb).exists() {
        // Safety: the suite runner is effectively single-threaded at
        // this point (steps run sequentially).
        unsafe { std::env::set_var("PATH", format!("{path}:{orb}")) };
    }
    sh(root, "cargo build -q --release -p spg-oracle-runner")?;
    sh(
        root,
        "docker compose -f xtests/oracle/docker-compose.yml up -d --wait",
    )?;
    let legs = sh(root, "cargo run -q --release -p spg-oracle-runner -- all");
    let down = sh(
        root,
        "docker compose -f xtests/oracle/docker-compose.yml down -v",
    );
    let summary = legs?;
    down?;
    Ok(tail_lines(&summary, 4))
}

fn tail_lines(out: &str, n: usize) -> String {
    let lines: Vec<&str> = out.lines().collect();
    lines[lines.len().saturating_sub(n)..].join("\n")
}

/// full-tier `generative` — the S4.2 differ: 10^4 seeded statements,
/// three legs (embedded / simple / extended), zero divergence. The
/// seed derives from the runid's git sha so a red night replays
/// exactly (`spg-gendiff --seed <printed>`).
///
/// # Errors
/// Build failure, or any divergence (drafts land in 15_regressions).
pub fn generative(root: &Path, runid: &str) -> Result<String, String> {
    sh(
        root,
        "cargo build -q --release -p spg-server -p spg-gendiff",
    )?;
    let seed = runid
        .bytes()
        .fold(0u64, |h, b| h.wrapping_mul(131).wrapping_add(u64::from(b)));
    // 7.38.1 S6.1 (D8) — the live-PG fourth leg rides whenever the
    // oracle container is reachable. 10^4 is the CP judgement; 10^5 is
    // the nightly parameter (SPG_GENDIFF_COUNT overrides).
    //
    // gendiff is a HOST binary dialing the oracle's published port, so
    // the leg is always 127.0.0.1 — `host.docker.internal` is a name
    // only resolvable INSIDE containers (the perf sweep uses it because
    // its psql wrapper runs in one; the first mini full to reach this
    // step aborted on that borrowed detection, 2026-08-19).
    let pg_host = "127.0.0.1";
    let count = std::env::var("SPG_GENDIFF_COUNT").unwrap_or_else(|_| String::from("10000"));
    sh(
        root,
        &format!(
            "SPG_GENDIFF_PG='{pg_host}:25432:bench:bench'              cargo run -q --release -p spg-gendiff -- --seed {seed} --count {count}"
        ),
    )
    .map(|out| tail_lines(&out, 2))
}

/// full-tier `sql2016` — the D16 coverage ledger's machine check: no
/// empty cells, every named corpus path exists, and the uncovered
/// count prints as the ledger that must shrink release over release.
///
/// # Errors
/// Malformed rows, unknown statuses, or a named corpus path that
/// doesn't exist (a moved file must move its ledger row with it).
pub fn sql2016(root: &Path) -> Result<String, String> {
    let path = root.join("xtests/sqllogictest/SQL2016-COVERAGE.tsv");
    let text =
        std::fs::read_to_string(&path).map_err(|e| format!("read {}: {e}", path.display()))?;
    let corpus = root.join("xtests/sqllogictest/corpus");
    let (mut covered, mut partial, mut uncovered) = (0usize, 0usize, 0usize);
    for (ln, line) in text.lines().enumerate() {
        if line.trim().is_empty() || line.starts_with('#') {
            continue;
        }
        let cells: Vec<&str> = line.split('\t').collect();
        if cells.len() != 5 || cells.iter().any(|c| c.trim().is_empty()) {
            return Err(format!(
                "SQL2016-COVERAGE.tsv:{}: need 5 non-empty cells",
                ln + 1
            ));
        }
        match cells[3] {
            "covered" => covered += 1,
            "partial" => partial += 1,
            "uncovered" => {
                uncovered += 1;
                if cells[4] != "-" {
                    return Err(format!(
                        "SQL2016-COVERAGE.tsv:{}: uncovered row must carry `-`",
                        ln + 1
                    ));
                }
                continue;
            }
            other => {
                return Err(format!(
                    "SQL2016-COVERAGE.tsv:{}: unknown status {other}",
                    ln + 1
                ));
            }
        }
        if !corpus.join(cells[4]).exists() {
            return Err(format!(
                "SQL2016-COVERAGE.tsv:{}: corpus path {} does not exist",
                ln + 1,
                cells[4]
            ));
        }
    }
    Ok(format!(
        "sql2016 ledger: covered={covered} partial={partial} uncovered={uncovered}"
    ))
}

/// full-tier `pgbench` (7.38 S5.1, D21) — pgbench's builtin tpcb-like
/// against SPGS over the wire, with a same-machine PG18 control leg
/// (the bench container runs both client and control server). The
/// drop-in bar: `pgbench -i` + the run COMPLETE, and the single-client
/// leg finishes with ZERO failed transactions. The contended leg's
/// failure rate prints into the account — the RC concurrent-UPDATE
/// blocking gap is ledgered (MATRIX #20), not hidden by this step.
///
/// # Errors
/// Server/build failure, init failure, or a single-client failure
/// count above zero.
pub fn pgbench(root: &Path, runid: &str) -> Result<String, String> {
    let bin = root.join("target/release/spg-server");
    if !bin.exists() {
        sh(root, "cargo build --release -q -p spg-server")?;
    }
    let tmp = crate::proclib::run_tmp_dir(&format!("{runid}-pgb"));
    let _ = std::fs::remove_dir_all(&tmp);
    let mut roster = Roster::new();
    let port = roster.spawn_server_on(
        "pgbench-leg",
        &bin,
        &tmp,
        Duration::from_secs(20),
        "0.0.0.0",
    )?;
    let orb = "/Applications/OrbStack.app/Contents/MacOS/xbin";
    let docker = if std::path::Path::new(orb).exists() {
        format!("PATH=\"$PATH:{orb}\" docker")
    } else {
        "docker".to_string()
    };
    let spg_uri = format!("postgres://bench:bench@host.docker.internal:{port}/bench");
    let grade = |out: &str| -> (String, String) {
        let pick = |pat: &str| {
            out.lines()
                .find(|l| l.contains(pat))
                .unwrap_or("(missing)")
                .trim()
                .to_string()
        };
        (pick("tps ="), pick("failed transactions"))
    };
    sh(
        root,
        &format!("{docker} exec spg-bench-postgres pgbench -i -s 1 -q '{spg_uri}'"),
    )?;
    let solo = sh(
        root,
        &format!("{docker} exec spg-bench-postgres pgbench -c 1 -T 10 '{spg_uri}'"),
    )?;
    let (solo_tps, solo_failed) = grade(&solo);
    let cont = sh(
        root,
        &format!("{docker} exec spg-bench-postgres pgbench -c 4 -j 2 -T 10 '{spg_uri}'"),
    )?;
    let (cont_tps, cont_failed) = grade(&cont);
    // Control leg: PG18 inside the same container (its own server).
    sh(
        root,
        &format!("{docker} exec spg-bench-postgres pgbench -i -s 1 -q -U bench bench"),
    )?;
    let pg = sh(
        root,
        &format!("{docker} exec spg-bench-postgres pgbench -c 1 -T 10 -U bench bench"),
    )?;
    let (pg_tps, _) = grade(&pg);
    roster.reap_all();
    let _ = std::fs::remove_dir_all(&tmp);
    if !solo_failed.contains("0 (0.000%)") {
        return Err(format!(
            "pgbench single-client leg had failures: {solo_failed}"
        ));
    }
    Ok(format!(
        "tpcb s=1: SPG c1 [{solo_tps}] vs PG18 c1 [{pg_tps}]; SPG c4 [{cont_tps}, {cont_failed} — MATRIX #20]"
    ))
}

/// full-tier `sysbench` (7.38 S5.2, D21) — the MySQL-dialect leg:
/// sysbench oltp_read_write over SPG's mysql wire (zero ignored
/// errors required), a same-machine MySQL control leg via the D13
/// oracle image, and — when the Percona tpcc scripts are present at
/// /tmp/sysbench-tpcc — a tpcc leg too (absence is a loud note, not
/// a silent skip). Needs a native `sysbench` on the runner.
///
/// # Errors
/// Missing sysbench, server failure, or any leg with errors.
pub fn sysbench(root: &Path, runid: &str) -> Result<String, String> {
    let sysbench = ["/opt/homebrew/bin/sysbench", "/usr/local/bin/sysbench"]
        .iter()
        .find(|p| std::path::Path::new(p).exists())
        .map(|p| (*p).to_string())
        .ok_or("sysbench not installed on this runner (brew install sysbench)")?;
    let bin = root.join("target/release/spg-server");
    if !bin.exists() {
        sh(root, "cargo build --release -q -p spg-server")?;
    }
    let tmp = crate::proclib::run_tmp_dir(&format!("{runid}-sb"));
    let _ = std::fs::remove_dir_all(&tmp);
    // The mysql wire rides an env var, so spawn with it set.
    let mut roster = Roster::new();
    let my_port = 25459; // one below the suite pg range; probed by bind
    if std::net::TcpListener::bind(("127.0.0.1", my_port)).is_err() {
        return Err("port 25459 (mysql-wire leg) is taken — janitor time".into());
    }
    let _pg = roster.spawn_server_env(
        "sysbench-leg",
        &bin,
        &tmp,
        Duration::from_secs(20),
        "127.0.0.1",
        &[("SPG_MYSQLWIRE_ADDR", "127.0.0.1:25459")],
    )?;
    let uri = format!(
        "--mysql-host=127.0.0.1 --mysql-port={my_port} --mysql-user=bench --mysql-password=bench --mysql-db=bench"
    );
    sh(
        root,
        &format!("{sysbench} oltp_read_write {uri} --tables=2 --table-size=1000 prepare"),
    )?;
    let run = sh(
        root,
        &format!(
            "{sysbench} oltp_read_write {uri} --tables=2 --table-size=1000 --threads=1 --time=10 run"
        ),
    )?;
    let pick = |out: &str, pat: &str| {
        out.lines()
            .find(|l| l.trim_start().starts_with(pat))
            .unwrap_or("(missing)")
            .trim()
            .to_string()
    };
    let spg_tx = pick(&run, "transactions:");
    let spg_err = pick(&run, "ignored errors:");
    if !spg_err.contains("0      (0.00 per sec.)")
        && !spg_err.contains("ignored errors:                      0")
    {
        return Err(format!("sysbench SPG leg had errors: {spg_err}"));
    }
    // tpcc leg — the Percona scripts self-fetch to a pinned commit in
    // the user cache (7.38.1 S1.4, D10②): no /tmp inheritance, no
    // floating upstream. Offline runners without the cache get a loud
    // note, never a silent skip.
    const TPCC_PIN: &str = "f110afa8023c7924b1ba00177232a9090624acb5";
    let tpcc_dir = std::env::var("HOME")
        .map(|h| std::path::PathBuf::from(h).join(".cache/spg-suite/sysbench-tpcc"))
        .map_err(|_| "no $HOME")?;
    if !tpcc_dir.join("tpcc.lua").exists() {
        let _ = sh(
            root,
            &format!(
                "git clone -q https://github.com/Percona-Lab/sysbench-tpcc {} && git -C {} checkout -q {TPCC_PIN}",
                tpcc_dir.display(),
                tpcc_dir.display()
            ),
        );
    } else {
        // Present but drifted? Pin it back — the corpus is a contract.
        let _ = sh(
            root,
            &format!("git -C {} checkout -q {TPCC_PIN}", tpcc_dir.display()),
        );
    }
    let tpcc_note = if tpcc_dir.join("tpcc.lua").exists() {
        sh(
            root,
            &format!(
                "cd {} && {sysbench} ./tpcc.lua {uri} --tables=1 --scale=1 --use_fk=0 prepare",
                tpcc_dir.display()
            ),
        )?;
        let t = sh(
            root,
            &format!(
                "cd {} && {sysbench} ./tpcc.lua {uri} --tables=1 --scale=1 --use_fk=0 --threads=1 --time=10 run",
                tpcc_dir.display()
            ),
        )?;
        format!("; tpcc [{}]", pick(&t, "transactions:"))
    } else {
        format!(
            "; tpcc UNAVAILABLE (clone failed and no cache at {})",
            tpcc_dir.display()
        )
    };
    roster.reap_all();
    let _ = std::fs::remove_dir_all(&tmp);
    // Control leg — the D13 mysql oracle image, same client, same
    // shape. Best-effort: a control that can't start is a note, not
    // a red (the SPG leg above is the gate).
    let orb = "/Applications/OrbStack.app/Contents/MacOS/xbin";
    let docker = if std::path::Path::new(orb).exists() {
        format!("PATH=\"$PATH:{orb}\" docker")
    } else {
        "docker".to_string()
    };
    let control = (|| -> Result<String, String> {
        sh(
            root,
            &format!("{docker} compose -f xtests/oracle/docker-compose.yml up -d --wait mysql"),
        )?;
        let curi = "--mysql-host=127.0.0.1 --mysql-port=53306 --mysql-user=root --mysql-password=testpass --mysql-db=testdb";
        sh(
            root,
            &format!("{sysbench} oltp_read_write {curi} --tables=2 --table-size=1000 prepare"),
        )?;
        let r = sh(
            root,
            &format!(
                "{sysbench} oltp_read_write {curi} --tables=2 --table-size=1000 --threads=1 --time=10 run"
            ),
        );
        let _ = sh(
            root,
            &format!("{docker} compose -f xtests/oracle/docker-compose.yml down -v"),
        );
        r.map(|out| pick(&out, "transactions:"))
    })();
    let control_note = match control {
        Ok(tx) => format!("; MySQL control [{tx}]"),
        Err(e) => format!(
            "; MySQL control UNAVAILABLE ({})",
            e.lines().next().unwrap_or("")
        ),
    };
    Ok(format!(
        "oltp_read_write SPG [{spg_tx}]{tpcc_note}{control_note}"
    ))
}

/// full-tier `pgdump-roundtrip` (7.38.1 S5.2, D6) — the official
/// PG18 pg_dump runs against a live SPG carrying the rich shape set,
/// must EXIT 0, and the dump must restore into a FRESH SPG and into a
/// FRESH PG18 database with the same row counts on a canary query.
/// pg_dump comes from the oracle container (mini) or the host
/// toolchain (local), same detection the sweep uses for psql.
///
/// # Errors
/// pg_dump non-zero, any restore error on the SPG leg, or a count
/// mismatch across the three sides.
pub fn pgdump_roundtrip(root: &Path, runid: &str) -> Result<String, String> {
    let bin = root.join("target/release/spg-server");
    if !bin.exists() {
        sh(root, "cargo build --release -q -p spg-server")?;
    }
    let home = std::env::var("HOME").unwrap_or_default();
    let wrapper = Path::new(&home).join("spgbench/bin/psql");
    let orb = "/Applications/OrbStack.app/Contents/MacOS/xbin";
    let docker = if std::path::Path::new(orb).exists() {
        format!("PATH=\"$PATH:{orb}\" docker")
    } else {
        "docker".to_string()
    };
    let (psql, pg_dump, host, bind) = if wrapper.exists() {
        (
            wrapper.display().to_string(),
            format!("{docker} exec spg-bench-postgres pg_dump"),
            "host.docker.internal",
            "0.0.0.0",
        )
    } else {
        (
            "psql".to_string(),
            "pg_dump".to_string(),
            "127.0.0.1",
            "127.0.0.1",
        )
    };
    let tmp = crate::proclib::run_tmp_dir(&format!("{runid}-pgdumprt"));
    let _ = std::fs::remove_dir_all(&tmp);
    let mut roster = Roster::new();
    let src = roster.spawn_server_on(
        "dump-src",
        &bin,
        &tmp.join("src"),
        Duration::from_secs(30),
        bind,
    )?;
    let dst = roster.spawn_server_on(
        "dump-dst",
        &bin,
        &tmp.join("dst"),
        Duration::from_secs(30),
        bind,
    )?;
    let src_uri = format!("postgres://bench:bench@{host}:{src}/bench");
    let dst_uri = format!("postgres://bench:bench@{host}:{dst}/bench");
    const RICH: &str = "CREATE TABLE rich1 (id BIGSERIAL PRIMARY KEY, tag TEXT[] DEFAULT '{}', \
         amt NUMERIC(12,3), payload JSONB, blob BYTEA, flag BOOLEAN DEFAULT false, \
         created TIMESTAMPTZ DEFAULT now()); \
         CREATE TABLE rich2 (id INT PRIMARY KEY, r1 BIGINT REFERENCES rich1(id) ON DELETE CASCADE, \
         uq TEXT UNIQUE, CHECK (id > 0)); \
         CREATE TYPE addr AS (street TEXT, zip INT); \
         CREATE TABLE rich3 (id INT PRIMARY KEY, home addr, mood_col TEXT); \
         CREATE MATERIALIZED VIEW mv1 AS SELECT count(*) AS n FROM rich1; \
         CREATE TABLE part_parent (id INT, ts DATE) PARTITION BY RANGE (ts); \
         CREATE TABLE part_a PARTITION OF part_parent FOR VALUES FROM ('2026-01-01') TO ('2026-06-01'); \
         CREATE INDEX rich1_gin ON rich1 USING gin (payload); \
         INSERT INTO rich1 (tag, amt, payload, blob) VALUES ('{a,b}', 12.345, '{\"k\":1}', '\\xdeadbeef'); \
         INSERT INTO rich2 VALUES (1, 1, 'x'); \
         INSERT INTO rich3 VALUES (1, ROW('main st', 12345), 'ok'); \
         INSERT INTO part_parent VALUES (1, '2026-02-01');";
    const CANARY: &str = "SELECT (SELECT count(*) FROM rich1) || '|' || \
         (SELECT count(*) FROM rich2) || '|' || (SELECT count(*) FROM rich3) || '|' || \
         (SELECT count(*) FROM part_parent) || '|' || (SELECT (home).zip FROM rich3)";
    let schema_file = tmp.join("rich-schema.sql");
    std::fs::write(&schema_file, RICH).map_err(|e| format!("write schema: {e}"))?;
    sh(
        root,
        &format!(
            "{psql} --no-psqlrc -X -q '{src_uri}' -f - < {}",
            schema_file.display()
        ),
    )?;
    let dump_file = tmp.join("rich-dump.sql");
    sh(
        root,
        &format!("{pg_dump} '{src_uri}' > {}", dump_file.display()),
    )
    .map_err(|e| format!("pg_dump must exit 0 against SPG: {e}"))?;
    // Leg 1 — fresh SPG. Any ERROR line is a red.
    let restore = sh(
        root,
        &format!(
            "{psql} --no-psqlrc -X -q '{dst_uri}' -f - < {} 2>&1 | grep -c ERROR || true",
            dump_file.display()
        ),
    )?;
    if restore.trim() != "0" {
        return Err(format!(
            "SPG restore leg had {} error line(s)",
            restore.trim()
        ));
    }
    let src_counts = sh(
        root,
        &format!("{psql} --no-psqlrc -X -q -tA '{src_uri}' -c \"{CANARY}\""),
    )?;
    let dst_counts = sh(
        root,
        &format!("{psql} --no-psqlrc -X -q -tA '{dst_uri}' -c \"{CANARY}\""),
    )?;
    if src_counts.trim() != dst_counts.trim() {
        return Err(format!(
            "SPG roundtrip counts diverge: src={} dst={}",
            src_counts.trim(),
            dst_counts.trim()
        ));
    }
    // Leg 2 — fresh PG18 in the oracle container (skipped, loudly,
    // when no oracle container is reachable — the LOCAL box drives
    // its docker PG through the same 25432 bench container).
    let pg_admin = format!("postgres://bench:bench@{host}:25432/postgres");
    let pg_rt = format!("postgres://bench:bench@{host}:25432/spgdumprt");
    let pg_leg = sh(
        root,
        &format!(
            "{psql} --no-psqlrc -X -q -tA '{pg_admin}' -c 'DROP DATABASE IF EXISTS spgdumprt' \
             -c 'CREATE DATABASE spgdumprt' && \
             {psql} --no-psqlrc -X -q '{pg_rt}' -f - < {} >/dev/null 2>&1; \
             {psql} --no-psqlrc -X -q -tA '{pg_rt}' -c \"{CANARY}\"",
            dump_file.display()
        ),
    );
    let verdict = match pg_leg {
        Ok(pg_counts) if pg_counts.trim() == src_counts.trim() => {
            format!("three-way OK counts={}", src_counts.trim())
        }
        Ok(pg_counts) => {
            return Err(format!(
                "PG18 leg counts diverge: src={} pg={}",
                src_counts.trim(),
                pg_counts.trim()
            ));
        }
        Err(e) => return Err(format!("PG18 leg failed: {e}")),
    };
    roster.reap_all();
    let _ = std::fs::remove_dir_all(&tmp);
    Ok(verdict)
}
