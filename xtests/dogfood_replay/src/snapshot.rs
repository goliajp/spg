//! Snapshot loader — SHA-256 verify + tarball extract.
//!
//! Snapshots are kept *out of git* (they are routinely >100 MB).
//! The framework expects the tarball to live next to `fixture.json`
//! at the path the descriptor names. SHA-256 is checked before
//! extraction; if the file is missing the caller falls back to
//! `SkippedReason::SnapshotMissing` and the runner records it as
//! `SKIP` (not `FAIL`) — fixtures that need a prod snapshot
//! shouldn't gate CI on a checked-out repo with no snapshots.

use anyhow::{Context, Result, anyhow};
use sha2::{Digest, Sha256};
use std::fs::File;
use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::Command;
use tempfile::TempDir;

/// Outcome of `verify_snapshot` — file missing is *not* an error.
pub enum SnapshotState {
    /// Tarball is present and SHA-256 matches the descriptor.
    Present { path: PathBuf, size_bytes: u64 },
    /// Tarball does not exist on disk.
    Missing,
    /// Tarball exists but SHA-256 does not match — corrupt or
    /// stale download.
    Corrupted {
        path: PathBuf,
        actual_sha256: String,
        expected_sha256: String,
    },
}

/// Verify the snapshot tarball matches the descriptor SHA-256. Does
/// not extract.
pub fn verify_snapshot(
    fixture_dir: &Path,
    rel_path: &str,
    expected_sha256: &str,
) -> Result<SnapshotState> {
    let tarball = fixture_dir.join(rel_path);
    if !tarball.exists() {
        return Ok(SnapshotState::Missing);
    }
    let actual =
        sha256_of_file(&tarball).with_context(|| format!("sha256 of {}", tarball.display()))?;
    if !actual.eq_ignore_ascii_case(expected_sha256) {
        return Ok(SnapshotState::Corrupted {
            path: tarball.clone(),
            actual_sha256: actual,
            expected_sha256: expected_sha256.to_string(),
        });
    }
    let size = std::fs::metadata(&tarball)?.len();
    Ok(SnapshotState::Present {
        path: tarball,
        size_bytes: size,
    })
}

fn sha256_of_file(path: &Path) -> Result<String> {
    let mut hasher = Sha256::new();
    let mut reader = BufReader::new(File::open(path)?);
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

/// A snapshot that has been extracted into a temp dir; the dir is
/// cleaned on `Drop`.
pub struct ExtractedSnapshot {
    /// Temp dir holding the extracted catalog. Kept alive so the
    /// directory survives until the runner is done.
    _tmp: TempDir,
    /// Path inside the temp dir that should be passed to
    /// `Database::open_path` — guessed from the tarball contents
    /// (single `.spg` directory at the top level).
    pub catalog_path: PathBuf,
}

/// Extract a verified-present snapshot into a fresh temp dir and
/// locate the `.spg` catalog directory inside it.
///
/// Uses the host's `tar` binary — there is no native `tar` crate
/// in the workspace lockfile. macOS/Linux both ship a usable `tar`.
pub fn extract_snapshot(tarball: &Path) -> Result<ExtractedSnapshot> {
    let tmp = tempfile::Builder::new()
        .prefix("spg-dogfood-")
        .tempdir()
        .context("create temp dir for snapshot extract")?;

    let status = Command::new("tar")
        .arg("-xzf")
        .arg(tarball)
        .arg("-C")
        .arg(tmp.path())
        .status()
        .context("spawn `tar -xzf` for snapshot")?;
    if !status.success() {
        return Err(anyhow!(
            "tar -xzf {} -C {} failed with exit code {:?}",
            tarball.display(),
            tmp.path().display(),
            status.code()
        ));
    }

    let catalog_path = locate_catalog(tmp.path())?;
    // Drop any stale lock dir — the kill-9 scenario reproduces
    // exactly this state, but the lock-dir convention is "delete
    // before retry".
    let lock_dir = format!("{}.lock", catalog_path.display());
    let _ = std::fs::remove_dir_all(&lock_dir);

    Ok(ExtractedSnapshot {
        _tmp: tmp,
        catalog_path,
    })
}

fn locate_catalog(root: &Path) -> Result<PathBuf> {
    // Heuristic: walk one level deep looking for a `*.spg`
    // directory (the standard SPG on-disk layout). If nothing
    // matches, fall back to the root itself. NOTE: SPG's open_path
    // takes the catalog DIR — for older catalogs the on-disk shape was
    // `mailrs.spg` as a regular file (with sibling `mailrs.spg.wal/`
    // directory). In that case open_path takes the parent dir, not
    // the file. So we accept *either* a `*.spg` directory or a `*.spg`
    // regular file's parent as the open-path target.
    for entry in std::fs::read_dir(root)
        .with_context(|| format!("scan extracted snapshot {}", root.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        let name_ok = path
            .file_name()
            .and_then(|s| s.to_str())
            .is_some_and(|s| s.ends_with(".spg"));
        if name_ok {
            // open_path takes the catalog db path itself (regardless
            // of whether it's a directory or file). It derives the WAL
            // dir by appending `.wal` to the file_name. So we return
            // the actual *.spg path in both cases.
            return Ok(path);
        }
        // Recurse one level — some tarballs nest under a top dir.
        if path.is_dir()
            && let Ok(inner) = std::fs::read_dir(&path)
        {
            for sub in inner.flatten() {
                let sub_path = sub.path();
                let sub_name_ok = sub_path
                    .file_name()
                    .and_then(|s| s.to_str())
                    .is_some_and(|s| s.ends_with(".spg"));
                if sub_name_ok {
                    return Ok(sub_path);
                }
            }
        }
    }
    Err(anyhow!(
        "no *.spg catalog directory or file found under extracted snapshot at {}",
        root.display()
    ))
}
