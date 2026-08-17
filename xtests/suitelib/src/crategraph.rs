//! Workspace crate graph — the map behind `clippy-affected` and
//! `unit-affected` (design D4).
//!
//! Generated from the workspace's own Cargo.toml files (member list +
//! each member's `[dependencies]` on sibling members), written to
//! `xtests/suitelib/crate-graph.toml`, and stamped with a hash of
//! every Cargo.toml it read. A tier run recomputes the hash and
//! REFUSES to use a stale graph — the rsync/mtime family taught this
//! repo what silently-stale inputs cost.
//!
//! The base crates whose change upgrades precommit to full-workspace
//! unit (D4): `spg-storage`, `spg-sql` — everything sits on them.

use std::collections::BTreeMap;
use std::path::Path;

pub const GRAPH_PATH: &str = "xtests/suitelib/crate-graph.toml";
pub const BASE_CRATES: [&str; 2] = ["spg-storage", "spg-sql"];

#[derive(Debug, Default)]
pub struct CrateGraph {
    /// crate name → the workspace crates it depends on.
    pub deps: BTreeMap<String, Vec<String>>,
    /// crate name → its directory relative to the repo root.
    pub dirs: BTreeMap<String, String>,
    pub hash: u64,
}

/// FNV-1a over all the manifests, order-fixed. Zero-dep and plenty for
/// a staleness stamp — this is not a security boundary.
fn fnv(chunks: &[(String, String)]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for (name, text) in chunks {
        for b in name.bytes().chain(text.bytes()) {
            h ^= u64::from(b);
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    h
}

fn member_dirs(root: &Path) -> Result<Vec<String>, String> {
    let ws = std::fs::read_to_string(root.join("Cargo.toml"))
        .map_err(|e| format!("crategraph: workspace Cargo.toml: {e}"))?;
    let mut out = Vec::new();
    let mut in_members = false;
    for line in ws.lines() {
        let t = line.trim();
        if t.starts_with("members") {
            in_members = true;
        }
        if in_members {
            if let Some(m) = t.split('"').nth(1) {
                out.push(m.to_string());
            }
            if t.contains(']') {
                break;
            }
        }
    }
    if out.is_empty() {
        return Err("crategraph: no workspace members found".into());
    }
    Ok(out)
}

impl CrateGraph {
    /// Build the graph by reading every member manifest.
    ///
    /// # Errors
    /// Unreadable manifests or an empty member list.
    pub fn generate(root: &Path) -> Result<Self, String> {
        let dirs = member_dirs(root)?;
        let mut manifests: Vec<(String, String)> = Vec::new();
        let mut names: Vec<(String, String)> = Vec::new(); // (name, dir)
        for d in &dirs {
            let text = std::fs::read_to_string(root.join(d).join("Cargo.toml"))
                .map_err(|e| format!("crategraph: {d}/Cargo.toml: {e}"))?;
            let name = text
                .lines()
                .find_map(|l| {
                    let t = l.trim();
                    t.strip_prefix("name")
                        .and_then(|r| r.trim_start().strip_prefix('='))
                        .and_then(|r| r.trim().strip_prefix('"'))
                        .and_then(|r| r.split('"').next())
                        .map(str::to_string)
                })
                .ok_or_else(|| format!("crategraph: {d}: no package name"))?;
            names.push((name.clone(), d.clone()));
            manifests.push((name, text));
        }
        // Workspace root manifest participates in the hash too: the
        // member list and shared dep versions live there.
        let ws_text = std::fs::read_to_string(root.join("Cargo.toml"))
            .map_err(|e| format!("crategraph: workspace Cargo.toml: {e}"))?;
        let mut hash_input = manifests.clone();
        hash_input.push(("<workspace>".into(), ws_text));
        let member_names: Vec<String> = names.iter().map(|(n, _)| n.clone()).collect();
        let mut g = CrateGraph {
            hash: fnv(&hash_input),
            ..Default::default()
        };
        for ((name, text), (_, dir)) in manifests.iter().zip(names.iter()) {
            let deps: Vec<String> = text
                .lines()
                .filter_map(|l| {
                    let key = l.trim().split(['=', ' ', '.']).next()?.to_string();
                    member_names.contains(&key).then_some(key)
                })
                .filter(|k| k != name)
                .collect();
            let mut deps = deps;
            deps.sort();
            deps.dedup();
            g.deps.insert(name.clone(), deps);
            g.dirs.insert(name.clone(), dir.clone());
        }
        Ok(g)
    }

    /// Crates to re-check when `changed` crates changed: the changed
    /// set plus every member that (transitively) depends on one of
    /// them. A base-crate change returns everything (D4).
    #[must_use]
    pub fn affected(&self, changed: &[String]) -> Vec<String> {
        if changed.iter().any(|c| BASE_CRATES.contains(&c.as_str())) {
            return self.deps.keys().cloned().collect();
        }
        let mut out: Vec<String> = changed.to_vec();
        loop {
            let before = out.len();
            for (name, deps) in &self.deps {
                if !out.contains(name) && deps.iter().any(|d| out.contains(d)) {
                    out.push(name.clone());
                }
            }
            if out.len() == before {
                break;
            }
        }
        out.sort();
        out
    }

    /// Serialize to the graph file format.
    #[must_use]
    pub fn to_toml(&self) -> String {
        let mut s = String::from("# generated by `suite-run gen-crate-graph` — do not edit\n");
        s.push_str(&format!("hash = {}\n", self.hash));
        for (name, deps) in &self.deps {
            s.push_str(&format!(
                "\n[[crate]]\nname = \"{name}\"\ndir = \"{}\"\ndeps = [{}]\n",
                self.dirs[name],
                deps.iter()
                    .map(|d| format!("\"{d}\""))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        s
    }

    /// The stored hash, read cheaply for the staleness guard.
    ///
    /// # Errors
    /// Missing or malformed graph file — the caller's message tells
    /// the operator to regenerate, never to proceed.
    pub fn stored_hash(root: &Path) -> Result<u64, String> {
        let text = std::fs::read_to_string(root.join(GRAPH_PATH)).map_err(|_| {
            format!("crategraph: {GRAPH_PATH} missing — run `suite-run gen-crate-graph`")
        })?;
        text.lines()
            .find_map(|l| l.strip_prefix("hash = "))
            .and_then(|h| h.trim().parse().ok())
            .ok_or_else(|| format!("crategraph: {GRAPH_PATH} has no hash line — regenerate"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo_root() -> std::path::PathBuf {
        // xtests/suitelib -> repo root
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .nth(2)
            .unwrap()
            .to_path_buf()
    }

    #[test]
    fn generates_a_graph_with_known_edges() {
        let g = CrateGraph::generate(&repo_root()).expect("generate");
        // spg-engine depends on spg-sql and spg-storage — bedrock facts.
        let e = &g.deps["spg-engine"];
        assert!(e.contains(&"spg-sql".to_string()), "{e:?}");
        assert!(e.contains(&"spg-storage".to_string()), "{e:?}");
        assert!(g.hash != 0);
    }

    #[test]
    fn affected_closure_and_base_upgrade() {
        let g = CrateGraph::generate(&repo_root()).expect("generate");
        let a = g.affected(&["spg-engine".to_string()]);
        assert!(
            a.contains(&"spg-server".to_string()),
            "server sits on engine: {a:?}"
        );
        assert!(
            !a.contains(&"spg-sql".to_string()),
            "deps don't flow backwards: {a:?}"
        );
        let all = g.affected(&["spg-storage".to_string()]);
        assert_eq!(all.len(), g.deps.len(), "base crate change = everything");
    }
}
