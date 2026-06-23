//! YugabyteDB-style fixture name dispatch.
//!
//! - `port.<name>.sql`  — ported from upstream PG regress; expected/
//!                        carries BOTH `.pg.out` and `.spg.out`. If
//!                        `.spg.out` exists it's an EXPECTED FAILURE
//!                        lock; SPG matches `.spg.out` until the
//!                        underlying bug closes, then `.spg.out` is
//!                        deleted and the runner enforces `.pg.out`.
//! - `orig.<name>.sql`  — SPG-original test for surface PG doesn't
//!                        have (or has differently). Single
//!                        `.spg.out` of record.
//! - `depd.<name>.sql`  — depended-on setup. Referenced by other
//!                        fixtures via the runner's resolve pass;
//!                        not run standalone.
//!
//! Convention is borrowed from YugabyteDB's `yb.port.*` triplet —
//! their three-bucket scheme cleanly separates "ported from
//! upstream (track that upstream)" from "added by us (own it
//! entirely)" from "wiring fixture". See yugabyte/yugabyte-db
//! `src/postgres/src/test/regress/yb_lite_schedule` for prior art.

use anyhow::{Context, Result, anyhow, bail};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Three buckets that a fixture file falls into based on its
/// filename prefix.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Kind {
    Port,
    Orig,
    Depd,
}

impl Kind {
    pub fn prefix(self) -> &'static str {
        match self {
            Kind::Port => "port.",
            Kind::Orig => "orig.",
            Kind::Depd => "depd.",
        }
    }
}

/// Classify a fixture by filename prefix. Any file in `corpus/` that
/// doesn't match one of the three is a CI fail — naming convention
/// is enforced, not advisory.
pub fn classify(path: &Path) -> Result<Kind> {
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .context("non-utf8 fixture name")?;
    if name.starts_with("port.") {
        Ok(Kind::Port)
    } else if name.starts_with("orig.") {
        Ok(Kind::Orig)
    } else if name.starts_with("depd.") {
        Ok(Kind::Depd)
    } else {
        Err(anyhow!(
            "fixture `{name}` missing port./orig./depd. prefix — see xtests/oracle/README.md"
        ))
    }
}

/// Pair of expected/ paths for a given fixture + oracle.
#[derive(Debug)]
pub struct ExpectedPair {
    /// Oracle-side baseline (eg. `port.foo.pg.out`). Must exist.
    pub oracle: PathBuf,
    /// SPG-side override (eg. `port.foo.spg.out`). EXPECTED FAILURE
    /// lock — present means "SPG diverges from the oracle here and
    /// we know about it; track via backlog reference in the file".
    /// Absent means "SPG matches the oracle".
    pub spg: PathBuf,
}

/// Resolve the expected/ pair for a fixture against a given oracle.
pub fn expected_paths(
    fixture: &Path,
    expected_root: &Path,
    oracle: crate::dialect::Oracle,
) -> ExpectedPair {
    let stem = fixture
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("unnamed");
    ExpectedPair {
        oracle: expected_root.join(format!("{stem}.{}.out", oracle.suffix())),
        spg: expected_root.join(format!("{stem}.spg.out")),
    }
}

/// Walk corpus, group by `Kind`. Used by `spg-oracle-runner list`.
pub fn list(corpus_root: &Path) -> Result<BTreeMap<Kind, Vec<PathBuf>>> {
    if !corpus_root.exists() {
        bail!(
            "corpus root `{}` does not exist — run from repo root or pass --corpus",
            corpus_root.display()
        );
    }
    let mut grouped: BTreeMap<Kind, Vec<PathBuf>> = BTreeMap::new();
    for entry in walkdir::WalkDir::new(corpus_root)
        .into_iter()
        .filter_map(std::result::Result::ok)
    {
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.into_path();
        // Skip README and other non-fixture files.
        let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
            continue;
        };
        if ext != "sql" && ext != "slt" && ext != "test" {
            continue;
        }
        let kind = classify(&path)?;
        grouped.entry(kind).or_default().push(path);
    }
    for files in grouped.values_mut() {
        files.sort();
    }
    Ok(grouped)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_recognises_three_buckets() {
        assert_eq!(
            classify(Path::new("port.select_implicit.sql")).unwrap(),
            Kind::Port
        );
        assert_eq!(
            classify(Path::new("orig.jsonb_set.sql")).unwrap(),
            Kind::Orig
        );
        assert_eq!(
            classify(Path::new("depd.setup_users.sql")).unwrap(),
            Kind::Depd
        );
    }

    #[test]
    fn classify_rejects_unprefixed() {
        let err = classify(Path::new("select_implicit.sql")).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("port./orig./depd. prefix"));
    }
}
