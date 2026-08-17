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
