//! Permutation schema + bespoke TOML subset parser.
//!
//! We avoid pulling `toml` / `serde` as build-time deps so the perm-runner
//! stays a thin skeleton (per v7.38 plan section six fast-tier budget:
//! "runner itself must not pull large build deps"). The TOML grammar we
//! accept is the strict subset we emit in `tests/permutations.toml`:
//!
//!   * top-level `[default]` table
//!   * repeated `[[permutation]]` array-of-tables
//!   * inline-table values for `env` (e.g. `env = { K = "v" }`)
//!   * string, integer, bool, array-of-strings scalars
//!   * `#` line comments
//!
//! This is forward-compatible with `cargo`'s TOML: anything we write here
//! parses as standard TOML if someone later swaps the parser back to the
//! `toml` crate.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Embedded,
    Server,
}

impl Mode {
    fn parse(raw: &str) -> Result<Self, String> {
        match raw {
            "embedded" => Ok(Self::Embedded),
            "server" => Ok(Self::Server),
            other => Err(format!("unknown mode `{other}` — want embedded|server")),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Permutation {
    pub name: String,
    pub mode: Mode,
    pub env: BTreeMap<String, String>,
    pub full_only: bool,
}

#[derive(Debug, Clone)]
pub struct DefaultCfg {
    pub corpus_root: String,
    pub include_globs: Vec<String>,
    pub fast_tier: Vec<String>,
    pub fast_tier_sample: String,
    pub timeout_secs: u64,
}

#[derive(Debug, Clone)]
pub struct PermFile {
    pub default: DefaultCfg,
    pub permutations: Vec<Permutation>,
}

impl PermFile {
    pub fn load(path: &Path) -> Result<Self, String> {
        let src = fs::read_to_string(path).map_err(|e| format!("read {}: {e}", path.display()))?;
        parse(&src)
    }

    pub fn fast_tier_names(&self) -> &[String] {
        &self.default.fast_tier
    }

    pub fn by_name(&self, name: &str) -> Option<&Permutation> {
        self.permutations.iter().find(|p| p.name == name)
    }

    pub fn non_full_only(&self) -> impl Iterator<Item = &Permutation> {
        self.permutations.iter().filter(|p| !p.full_only)
    }
}

// ---------------------------------------------------------------------------
// Hand-rolled TOML subset parser
// ---------------------------------------------------------------------------

#[derive(Default)]
struct Builder {
    default: Option<DefaultCfg>,
    perms: Vec<Permutation>,
    // Scratch for the current table we are accumulating into.
    cur_table: Option<String>,
    cur_default_scratch: ScratchDefault,
    cur_perm_scratch: ScratchPerm,
}

#[derive(Default, Debug)]
struct ScratchDefault {
    corpus_root: Option<String>,
    include_globs: Option<Vec<String>>,
    fast_tier: Option<Vec<String>>,
    fast_tier_sample: Option<String>,
    timeout_secs: Option<u64>,
}

#[derive(Default, Debug)]
struct ScratchPerm {
    name: Option<String>,
    mode: Option<Mode>,
    env: BTreeMap<String, String>,
    full_only: bool,
}

fn parse(src: &str) -> Result<PermFile, String> {
    let mut b = Builder::default();
    for (lineno, raw) in src.lines().enumerate() {
        let line = strip_comment(raw).trim();
        if line.is_empty() {
            continue;
        }

        if let Some(name) = parse_table_header(line) {
            b.flush_current()?;
            b.cur_table = Some(name);
            continue;
        }

        // Otherwise this is a `key = value` line inside the current table.
        let (key, value) = parse_kv(line).map_err(|e| format!("line {}: {e}", lineno + 1))?;
        b.assign(&key, &value)
            .map_err(|e| format!("line {}: {e}", lineno + 1))?;
    }
    b.flush_current()?;

    let default = b
        .default
        .ok_or_else(|| "[default] table missing".to_string())?;
    if b.perms.is_empty() {
        return Err("at least one [[permutation]] required".into());
    }
    Ok(PermFile {
        default,
        permutations: b.perms,
    })
}

/// Returns `Some("default")` for `[default]` or `Some("__perm__")` for
/// `[[permutation]]`. We collapse the two array-of-tables forms because we
/// only have one of each (default once, permutation repeated).
fn parse_table_header(line: &str) -> Option<String> {
    if let Some(inner) = line.strip_prefix("[[").and_then(|s| s.strip_suffix("]]")) {
        return Some(format!("__array__:{}", inner.trim()));
    }
    if let Some(inner) = line.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
        return Some(inner.trim().to_string());
    }
    None
}

fn parse_kv(line: &str) -> Result<(String, String), String> {
    let (k, v) = line.split_once('=').ok_or("expected `key = value`")?;
    Ok((k.trim().to_string(), v.trim().to_string()))
}

fn strip_comment(raw: &str) -> &str {
    // A `#` inside a string literal would break this, but our schema
    // doesn't have any. Safe simplification.
    raw.find('#').map_or(raw, |i| &raw[..i])
}

impl Builder {
    fn assign(&mut self, key: &str, value: &str) -> Result<(), String> {
        let table = self.cur_table.as_deref().unwrap_or("");
        match table {
            "default" => self.assign_default(key, value),
            t if t.starts_with("__array__:permutation") => self.assign_perm(key, value),
            "" => Err(format!("key `{key}` outside any table")),
            other => Err(format!("unknown table `{other}`")),
        }
    }

    fn assign_default(&mut self, key: &str, value: &str) -> Result<(), String> {
        let s = &mut self.cur_default_scratch;
        match key {
            "corpus_root" => s.corpus_root = Some(parse_string(value)?),
            "include_globs" => s.include_globs = Some(parse_string_array(value)?),
            "fast_tier" => s.fast_tier = Some(parse_string_array(value)?),
            "fast_tier_sample" => s.fast_tier_sample = Some(parse_string(value)?),
            "timeout_secs" => s.timeout_secs = Some(parse_u64(value)?),
            other => return Err(format!("[default]: unknown key `{other}`")),
        }
        Ok(())
    }

    fn assign_perm(&mut self, key: &str, value: &str) -> Result<(), String> {
        let s = &mut self.cur_perm_scratch;
        match key {
            "name" => s.name = Some(parse_string(value)?),
            "mode" => s.mode = Some(Mode::parse(&parse_string(value)?)?),
            "env" => s.env = parse_inline_table(value)?,
            "full_only" => s.full_only = parse_bool(value)?,
            other => return Err(format!("[[permutation]]: unknown key `{other}`")),
        }
        Ok(())
    }

    fn flush_current(&mut self) -> Result<(), String> {
        let table = self.cur_table.take().unwrap_or_default();
        match table.as_str() {
            "default" => {
                let s = std::mem::take(&mut self.cur_default_scratch);
                let default = DefaultCfg {
                    corpus_root: s.corpus_root.ok_or("[default] missing corpus_root")?,
                    include_globs: s.include_globs.ok_or("[default] missing include_globs")?,
                    fast_tier: s.fast_tier.ok_or("[default] missing fast_tier")?,
                    fast_tier_sample: s
                        .fast_tier_sample
                        .ok_or("[default] missing fast_tier_sample")?,
                    timeout_secs: s.timeout_secs.unwrap_or(30),
                };
                self.default = Some(default);
            }
            t if t.starts_with("__array__:permutation") => {
                let s = std::mem::take(&mut self.cur_perm_scratch);
                let perm = Permutation {
                    name: s.name.ok_or("[[permutation]] missing name")?,
                    mode: s.mode.ok_or("[[permutation]] missing mode")?,
                    env: s.env,
                    full_only: s.full_only,
                };
                self.perms.push(perm);
            }
            "" => {}
            other => return Err(format!("unknown table `{other}`")),
        }
        Ok(())
    }
}

fn parse_string(raw: &str) -> Result<String, String> {
    let t = raw.trim();
    let stripped = t
        .strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .ok_or_else(|| format!("expected quoted string, got `{t}`"))?;
    Ok(stripped.to_string())
}

fn parse_bool(raw: &str) -> Result<bool, String> {
    match raw.trim() {
        "true" => Ok(true),
        "false" => Ok(false),
        other => Err(format!("expected bool, got `{other}`")),
    }
}

fn parse_u64(raw: &str) -> Result<u64, String> {
    raw.trim().parse().map_err(|e| format!("expected u64: {e}"))
}

fn parse_string_array(raw: &str) -> Result<Vec<String>, String> {
    let t = raw.trim();
    let inner = t
        .strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .ok_or_else(|| format!("expected array, got `{t}`"))?;
    if inner.trim().is_empty() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for elem in split_top_level(inner, ',') {
        out.push(parse_string(elem.trim())?);
    }
    Ok(out)
}

fn parse_inline_table(raw: &str) -> Result<BTreeMap<String, String>, String> {
    let t = raw.trim();
    let inner = t
        .strip_prefix('{')
        .and_then(|s| s.strip_suffix('}'))
        .ok_or_else(|| format!("expected inline table, got `{t}`"))?;
    let mut out = BTreeMap::new();
    if inner.trim().is_empty() {
        return Ok(out);
    }
    for elem in split_top_level(inner, ',') {
        let elem = elem.trim();
        if elem.is_empty() {
            continue;
        }
        let (k, v) = elem.split_once('=').ok_or("env entry missing `=`")?;
        out.insert(k.trim().to_string(), parse_string(v.trim())?);
    }
    Ok(out)
}

/// Split by `sep` but respect `"..."` quoting (no nested array/table support
/// needed in our schema).
fn split_top_level(s: &str, sep: char) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_str = false;
    for c in s.chars() {
        if c == '"' {
            in_str = !in_str;
            cur.push(c);
        } else if c == sep && !in_str {
            out.push(std::mem::take(&mut cur));
        } else {
            cur.push(c);
        }
    }
    if !cur.trim().is_empty() {
        out.push(cur);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_minimal_file() {
        let src = r#"
[default]
corpus_root = "xtests/sqllogictest/corpus"
include_globs = ["spg_baseline"]
fast_tier = ["embedded"]
fast_tier_sample = "spg_baseline/01_basic_dml"
timeout_secs = 30

[[permutation]]
name = "embedded"
mode = "embedded"
env  = { }

[[permutation]]
name = "topk_off"
mode = "embedded"
env  = { SPG_TEST_DISABLE_TOPK = "1" }
"#;
        let pf = parse(src).expect("parse");
        assert_eq!(pf.default.corpus_root, "xtests/sqllogictest/corpus");
        assert_eq!(pf.default.fast_tier, vec!["embedded"]);
        assert_eq!(pf.permutations.len(), 2);
        assert_eq!(pf.permutations[1].name, "topk_off");
        assert_eq!(
            pf.permutations[1].env.get("SPG_TEST_DISABLE_TOPK"),
            Some(&"1".to_string()),
        );
    }

    #[test]
    fn parses_shipped_permutations_toml() {
        let src = include_str!("../tests/permutations.toml");
        let pf = parse(src).expect("parse shipped file");
        assert!(pf.permutations.iter().any(|p| p.name == "embedded"));
        assert!(pf.permutations.iter().any(|p| p.name == "server_simple"));
        assert!(pf.permutations.iter().any(|p| p.name == "topk_off"));
        assert!(
            pf.permutations
                .iter()
                .any(|p| p.name == "debug_asserts" && p.full_only)
        );
    }
}
