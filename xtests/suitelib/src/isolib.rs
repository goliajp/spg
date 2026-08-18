//! Isolation harness (S4.1, design D14) — pg_isolationtester's shape
//! on SPG's own zero-dep stack: a spec file declares N sessions, named
//! steps, and explicit permutations; the runner spawns a REAL
//! spg-server per permutation (proclib), opens one pgwire connection
//! per session (wireclient), executes the schedule, and compares the
//! transcript against a blessed `.expected` file byte-for-byte.
//!
//! The spec format is the same loud TOML subset as the suite manifest:
//! `[iso]`, repeated `[[step]]`, repeated `[[permutation]]`; unknown
//! keys are errors, never guesses. Blocking interleavings are out of
//! scope for the first battery (wireclient's 10 s read timeout is the
//! backstop, and a timeout is a loud FAIL, not a hang).

use std::path::{Path, PathBuf};
use std::time::Duration;

#[derive(Debug, Default)]
pub struct IsoSpec {
    pub name: String,
    pub sessions: usize,
    pub steps: Vec<IsoStep>,
    pub permutations: Vec<Vec<String>>,
}

#[derive(Debug, Default, Clone)]
pub struct IsoStep {
    pub id: String,
    pub session: usize,
    pub sql: String,
    /// 7.38.1 S1.2 (D9) — send without reading: the statement is
    /// EXPECTED to block server-side; a later `wait` step harvests.
    pub async_send: bool,
    /// Harvest step: read the answer of the named async step's
    /// session. Mutually exclusive with `sql`.
    pub wait_for: Option<String>,
    /// Probe step: assert the named async step has NOT answered yet
    /// (200 ms MSG_PEEK). Transcript records `pending` or `answered`
    /// — a lock that fails to hold shows up as a diff, not a shrug.
    pub pending_of: Option<String>,
}

impl IsoSpec {
    /// Parse the loud TOML subset.
    ///
    /// # Errors
    /// Unknown sections/keys, steps referencing sessions out of range,
    /// permutations referencing unknown step ids, or a spec with no
    /// permutation — every one named with its line.
    pub fn parse(text: &str) -> Result<Self, String> {
        #[derive(PartialEq)]
        enum Sect {
            None,
            Iso,
            Step,
            Perm,
        }
        let mut out = IsoSpec::default();
        let mut sect = Sect::None;
        for (ln, raw) in text.lines().enumerate() {
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            match line {
                "[iso]" => {
                    sect = Sect::Iso;
                    continue;
                }
                "[[step]]" => {
                    sect = Sect::Step;
                    out.steps.push(IsoStep::default());
                    continue;
                }
                "[[permutation]]" => {
                    sect = Sect::Perm;
                    out.permutations.push(Vec::new());
                    continue;
                }
                _ => {}
            }
            let (k, v) = line
                .split_once('=')
                .ok_or_else(|| format!("iso spec:{}: expected key = value", ln + 1))?;
            let (k, v) = (k.trim(), v.trim());
            let unq = v.trim_matches('"').to_string();
            match (&sect, k) {
                (Sect::Iso, "name") => out.name = unq,
                (Sect::Iso, "sessions") => {
                    out.sessions = v
                        .parse()
                        .map_err(|e| format!("iso spec:{}: sessions: {e}", ln + 1))?;
                }
                (Sect::Step, "id") => {
                    out.steps.last_mut().expect("in [[step]]").id = unq;
                }
                (Sect::Step, "session") => {
                    out.steps.last_mut().expect("in [[step]]").session = v
                        .parse()
                        .map_err(|e| format!("iso spec:{}: session: {e}", ln + 1))?;
                }
                (Sect::Step, "sql") => {
                    out.steps.last_mut().expect("in [[step]]").sql = unq;
                }
                (Sect::Step, "async") => {
                    out.steps.last_mut().expect("in [[step]]").async_send = v == "1";
                }
                (Sect::Step, "wait") => {
                    out.steps.last_mut().expect("in [[step]]").wait_for = Some(unq);
                }
                (Sect::Step, "pending") => {
                    out.steps.last_mut().expect("in [[step]]").pending_of = Some(unq);
                }
                (Sect::Perm, "order") => {
                    *out.permutations.last_mut().expect("in [[permutation]]") =
                        unq.split_whitespace().map(str::to_string).collect();
                }
                _ => return Err(format!("iso spec:{}: unknown key {k}", ln + 1)),
            }
        }
        // Validation — a shrugged-off reference is a thinner battery.
        if out.name.is_empty() || out.sessions == 0 {
            return Err("iso spec: [iso] needs name and sessions".into());
        }
        if out.permutations.is_empty() {
            return Err(format!("iso spec {}: no [[permutation]]", out.name));
        }
        for s in &out.steps {
            // wait/pending steps borrow their target's session.
            if let Some(t) = s.wait_for.as_ref().or(s.pending_of.as_ref()) {
                if !out.steps.iter().any(|x| &x.id == t && x.async_send) {
                    return Err(format!(
                        "iso spec {}: step {} references async step {t} which does not exist",
                        out.name, s.id
                    ));
                }
                continue;
            }
            if s.session == 0 || s.session > out.sessions {
                return Err(format!(
                    "iso spec {}: step {} session {} out of range 1..={}",
                    out.name, s.id, s.session, out.sessions
                ));
            }
        }
        for p in &out.permutations {
            for id in p {
                if !out.steps.iter().any(|s| &s.id == id) {
                    return Err(format!(
                        "iso spec {}: permutation names unknown step {id}",
                        out.name
                    ));
                }
            }
        }
        Ok(out)
    }
}

/// One step's transcript line: `id: ok <tag>` / `id: rows a|b, c|d` /
/// `id: error <first line>`.
fn render_step(id: &str, r: &Result<crate::wireclient::QueryResult, String>) -> String {
    match r {
        Err(e) => format!("{id}: wire-error {e}"),
        Ok(q) => {
            if let Some(e) = &q.error {
                let first = e.lines().next().unwrap_or_default();
                format!("{id}: error {first}")
            } else if q.rows.is_empty() {
                format!(
                    "{id}: ok {}",
                    q.command_tags.last().map(String::as_str).unwrap_or("")
                )
            } else {
                let rows: Vec<String> = q.rows.iter().map(|r| r.join("|")).collect();
                format!("{id}: rows {}", rows.join(", "))
            }
        }
    }
}

/// Run every spec under `specs_dir`. With `bless`, transcripts are
/// written as the `.expected` files instead of compared.
///
/// # Errors
/// Spec parse failures, server/connection failures, or any transcript
/// diverging from its blessed expectation.
pub fn run_all(root: &Path, specs_dir: &Path, bless: bool) -> Result<String, String> {
    let bin = root.join("target/release/spg-server");
    if !bin.exists() {
        return Err("target/release/spg-server not built".into());
    }
    let mut entries: Vec<PathBuf> = std::fs::read_dir(specs_dir)
        .map_err(|e| format!("read {}: {e}", specs_dir.display()))?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "toml"))
        .collect();
    entries.sort();
    if entries.is_empty() {
        return Err(format!("no specs in {}", specs_dir.display()));
    }
    let mut total = 0usize;
    let mut failed: Vec<String> = Vec::new();
    for spec_path in &entries {
        let text = std::fs::read_to_string(spec_path)
            .map_err(|e| format!("read {}: {e}", spec_path.display()))?;
        let spec = IsoSpec::parse(&text)?;
        let transcript = run_spec(&bin, &spec)?;
        let expected_path = spec_path.with_extension("expected");
        total += 1;
        if bless {
            std::fs::write(&expected_path, &transcript)
                .map_err(|e| format!("write {}: {e}", expected_path.display()))?;
            println!(
                "iso BLESS {} <- {} bytes",
                expected_path.display(),
                transcript.len()
            );
            continue;
        }
        let expected = std::fs::read_to_string(&expected_path).map_err(|e| {
            format!(
                "read {} (run with --bless first): {e}",
                expected_path.display()
            )
        })?;
        if transcript != expected {
            eprintln!(
                "iso FAIL {}:\n--- expected\n{expected}\n--- actual\n{transcript}",
                spec.name
            );
            failed.push(spec.name.clone());
        }
    }
    if failed.is_empty() {
        Ok(format!("iso specs={total} failed=0"))
    } else {
        Err(format!(
            "iso specs={total} failed={}: {failed:?}",
            failed.len()
        ))
    }
}

fn run_spec(bin: &Path, spec: &IsoSpec) -> Result<String, String> {
    // 7.38.1 S1.2 — `SPG_ISO_TARGET=host:port:user:db` runs the specs
    // against an EXTERNAL server (the PG oracle container) instead of
    // spawning spg-server: the blocking specs take their behavioural
    // baseline from PG itself, clean-room style. External targets get
    // a per-permutation schema wipe instead of a fresh process.
    let target = std::env::var("SPG_ISO_TARGET").ok();
    let mut out = String::new();
    for (pi, perm) in spec.permutations.iter().enumerate() {
        let mut roster = crate::proclib::Roster::new();
        let mut tmp = PathBuf::new();
        let (host, port, user, db) = if let Some(t) = &target {
            let p: Vec<&str> = t.split(':').collect();
            if p.len() != 4 {
                return Err("SPG_ISO_TARGET wants host:port:user:db".into());
            }
            (
                p[0].to_string(),
                p[1].parse::<u16>()
                    .map_err(|e| format!("target port: {e}"))?,
                p[2].to_string(),
                p[3].to_string(),
            )
        } else {
            // A REAL server per permutation — schedules must not see
            // each other's state.
            tmp = crate::proclib::run_tmp_dir(&format!("iso-{}-{pi}", spec.name));
            let _ = std::fs::remove_dir_all(&tmp);
            let port = roster.spawn_server(&spec.name, bin, &tmp, Duration::from_secs(20))?;
            (
                "127.0.0.1".to_string(),
                port,
                "iso0".to_string(),
                "iso".to_string(),
            )
        };
        let mut conns: Vec<crate::wireclient::Conn> = Vec::new();
        for _ in 0..spec.sessions {
            conns.push(crate::wireclient::Conn::connect_host(
                &host, port, &user, &db,
            )?);
        }
        if target.is_some() {
            let r = conns[0].simple_query("DROP SCHEMA public CASCADE; CREATE SCHEMA public;")?;
            if let Some(e) = r.error {
                return Err(format!("external target schema wipe: {e}"));
            }
        }
        out.push_str(&format!("== permutation {}\n", perm.join(" ")));
        // Which async step's answer is in flight on each session.
        let mut in_flight: Vec<Option<String>> = vec![None; spec.sessions];
        for id in perm {
            let step = spec
                .steps
                .iter()
                .find(|s| &s.id == id)
                .expect("validated by parse");
            let session_of = |sid: &str| -> usize {
                spec.steps
                    .iter()
                    .find(|s| s.id == sid)
                    .expect("validated")
                    .session
                    - 1
            };
            if let Some(t) = &step.pending_of {
                let si = session_of(t);
                let line = match conns[si].poll_pending(200) {
                    Err(e) => format!("{id}: wire-error {e}"),
                    Ok(true) => format!("{id}: answered"),
                    Ok(false) => format!("{id}: pending"),
                };
                out.push_str(&line);
                out.push('\n');
                continue;
            }
            if let Some(t) = &step.wait_for {
                let si = session_of(t);
                let r = conns[si].read_result_deadline(Duration::from_secs(15));
                in_flight[si] = None;
                out.push_str(&render_step(id, &r));
                out.push('\n');
                continue;
            }
            let si = step.session - 1;
            if step.async_send {
                let line = match conns[si].send_query_nowait(&step.sql) {
                    Ok(()) => {
                        in_flight[si] = Some(step.id.clone());
                        format!("{id}: sent")
                    }
                    Err(e) => format!("{id}: wire-error {e}"),
                };
                out.push_str(&line);
                out.push('\n');
                continue;
            }
            let r = conns[si].simple_query(&step.sql);
            out.push_str(&render_step(id, &r));
            out.push('\n');
        }
        roster.reap_all();
        if target.is_none() {
            let _ = std::fs::remove_dir_all(&tmp);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spec_parses_and_validates() {
        let s = IsoSpec::parse(
            "[iso]\nname = \"t\"\nsessions = 2\n[[step]]\nid = \"a\"\nsession = 1\nsql = \"SELECT 1\"\n[[permutation]]\norder = \"a\"\n",
        )
        .unwrap();
        assert_eq!(s.name, "t");
        assert_eq!(s.steps.len(), 1);
        assert_eq!(s.permutations, vec![vec!["a".to_string()]]);
    }

    #[test]
    fn spec_rejects_unknown_step_in_permutation() {
        let e = IsoSpec::parse(
            "[iso]\nname = \"t\"\nsessions = 1\n[[step]]\nid = \"a\"\nsession = 1\nsql = \"SELECT 1\"\n[[permutation]]\norder = \"ghost\"\n",
        )
        .unwrap_err();
        assert!(e.contains("ghost"), "{e}");
    }

    #[test]
    fn spec_rejects_session_out_of_range() {
        let e = IsoSpec::parse(
            "[iso]\nname = \"t\"\nsessions = 1\n[[step]]\nid = \"a\"\nsession = 2\nsql = \"SELECT 1\"\n[[permutation]]\norder = \"a\"\n",
        )
        .unwrap_err();
        assert!(e.contains("out of range"), "{e}");
    }
}
