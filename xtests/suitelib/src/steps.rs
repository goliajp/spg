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
    sh(root, &format!("cargo test -q{flags} --lib --bins"))?;
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
    roster.reap_all();
    let _ = std::fs::remove_dir_all(&tmp);
    let text = out?;
    let verdict = text
        .lines()
        .rev()
        .find(|l| l.contains("losses="))
        .unwrap_or("(no verdict line)")
        .to_string();
    if verdict.contains("losses=0") {
        Ok(verdict)
    } else {
        Err(format!("sweep verdict: {verdict}"))
    }
}

/// `ironrules` (S1.3) — the prerelease tier's standing-rule step: the
/// wire smoke below, PLUS the previous release's data directory opened
/// directly by the CURRENT binary and verified row-for-row.
///
/// The fixture (`xtests/compat-datadirs/v7.37.29/`) was captured by
/// the v7.37.29 tag's own binary: 500 rows across nine types, two
/// indexes, deletes and updates, statement-level WAL (798 bytes, 7
/// records — there is no db file; replay IS the open). `expected.txt`
/// holds counts and an md5 over an ordered projection, so a silently
/// thinner replay cannot pass.
///
/// # Errors
/// Any probe or any fixture assertion failing, named.
pub fn ironrules_full(root: &Path, runid: &str) -> Result<String, String> {
    let smoke = ironrule_smoke(root, runid)?;
    let fixture = root.join("xtests/compat-datadirs/v7.37.29");
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
            return Err(format!("v7.37.29 fixture: {sql}: {e}"));
        }
        let got = r
            .rows
            .first()
            .and_then(|row| row.first())
            .cloned()
            .unwrap_or_default();
        if got != want {
            return Err(format!(
                "v7.37.29 fixture: {key}: want {want}, got {got} — the previous \
                 release's data did not survive the current binary"
            ));
        }
    }
    roster.reap_all();
    let _ = std::fs::remove_dir_all(&tmp);
    Ok(format!("{smoke}; v7.37.29 dir direct-open verified"))
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
    sh(
        root,
        &format!("cargo run -q --release -p spg-gendiff -- --seed {seed} --count 10000"),
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
