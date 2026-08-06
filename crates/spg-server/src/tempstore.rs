//! v7.39 (round 786, T35 Phase A) — the std-side spill storage the
//! engine cannot provide for itself.
//!
//! A run is one file under the temp directory. It is written once,
//! sealed, then read once — and removed when the handle drops, which is
//! what makes a cancelled or panicking query clean up after itself. The
//! process also sweeps leftovers at startup, because a `kill -9` gets no
//! `Drop`.

use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use spg_engine::{TempRun, TempStoreError};

/// Prefix every run file carries, so the startup sweep can recognise
/// its own leftovers and nothing else.
const RUN_PREFIX: &str = "spg-sort-";

static RUN_SERIAL: AtomicU64 = AtomicU64::new(0);

fn io_err(what: &str, e: &std::io::Error) -> TempStoreError {
    TempStoreError::Io(format!("{what}: {e}"))
}

/// Where runs live: `SPG_TEMP_DIR` when set, else the OS temp dir.
/// Kept as a function rather than a cached path so a test can point the
/// variable somewhere else between engines.
pub fn temp_dir() -> PathBuf {
    std::env::var_os("SPG_TEMP_DIR").map_or_else(std::env::temp_dir, PathBuf::from)
}

/// A single spill run backed by one file.
pub struct FileRun {
    pub(crate) path: PathBuf,
    file: File,
    written: u64,
    sealed: bool,
}

impl TempRun for FileRun {
    fn append(&mut self, bytes: &[u8]) -> Result<(), TempStoreError> {
        debug_assert!(!self.sealed, "append after seal");
        self.file
            .write_all(bytes)
            .map_err(|e| io_err("writing spill run", &e))?;
        self.written += bytes.len() as u64;
        Ok(())
    }

    fn seal(&mut self) -> Result<(), TempStoreError> {
        self.file
            .flush()
            .map_err(|e| io_err("flushing spill run", &e))?;
        // No fsync: a run is scratch. Losing it to a crash costs
        // nothing, because the query that owned it is gone too.
        self.file
            .seek(SeekFrom::Start(0))
            .map_err(|e| io_err("rewinding spill run", &e))?;
        self.sealed = true;
        Ok(())
    }

    fn read(&mut self, buf: &mut [u8]) -> Result<usize, TempStoreError> {
        debug_assert!(self.sealed, "read before seal");
        self.file
            .read(buf)
            .map_err(|e| io_err("reading spill run", &e))
    }

    fn bytes_written(&self) -> u64 {
        self.written
    }
}

impl Drop for FileRun {
    fn drop(&mut self) {
        // Best effort by construction: the startup sweep is the backstop
        // for the cases Drop cannot reach (SIGKILL, power loss).
        let _ = fs::remove_file(&self.path);
    }
}

/// The factory the engine holds. Matches `spg_engine::TempRunFactory`.
pub fn create_run() -> Result<Box<dyn TempRun>, TempStoreError> {
    Ok(Box::new(create_run_in(&temp_dir())?))
}

/// v7.39 (round 787) — the same run, with its directory as an explicit
/// argument instead of a hidden read of the environment.
///
/// The round-786 test counted entries in the process-wide temp
/// directory to prove a run creates and removes its file; under the
/// parallel suite other processes churn that directory constantly and
/// the count was never stable (the gate caught it at 230276 vs
/// 230273). A caller-supplied directory makes the observation exact,
/// and reading the environment in exactly one place is better shape
/// besides.
pub fn create_run_in(dir: &Path) -> Result<FileRun, TempStoreError> {
    fs::create_dir_all(dir).map_err(|e| io_err("creating temp dir", &e))?;
    let serial = RUN_SERIAL.fetch_add(1, Ordering::Relaxed);
    let path = dir.join(format!("{RUN_PREFIX}{}-{serial}.run", std::process::id()));
    let file = fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .read(true)
        .write(true)
        .open(&path)
        .map_err(|e| io_err("opening spill run", &e))?;
    Ok(FileRun {
        path,
        file,
        written: 0,
        sealed: false,
    })
}

/// Remove run files this process cannot own — anything matching the run
/// prefix whose pid is not ours. Called once at startup; a `kill -9`
/// leaves files that no `Drop` will ever reach.
pub fn sweep_orphans(dir: &Path) -> usize {
    let mine = format!("{RUN_PREFIX}{}-", std::process::id());
    let Ok(entries) = fs::read_dir(dir) else {
        return 0;
    };
    let mut removed = 0;
    for e in entries.flatten() {
        let name = e.file_name();
        let Some(name) = name.to_str() else { continue };
        if name.starts_with(RUN_PREFIX) && !name.starts_with(&mine) && fs::remove_file(e.path()).is_ok()
        {
            removed += 1;
        }
    }
    removed
}
