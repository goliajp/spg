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
/// v7.38.18 (G5) — the switch this reads, named once.
///
/// It lives here rather than beside the server's other switch names
/// because this module is also compiled into the e2e test binary, which
/// has a different crate root. See `AUTOVACUUM_ENV` in `main.rs` for
/// why the name is a const at all.
pub const TEMP_DIR_ENV: &str = "SPG_TEMP_DIR";

pub fn temp_dir() -> PathBuf {
    temp_dir_from(std::env::var_os(TEMP_DIR_ENV))
}

/// v7.38.18 (G5) — the placement decision, separated from the
/// environment so it can be tested.
///
/// The comment above has said since round 787 that this is "kept as a
/// function rather than a cached path so a test can point the variable
/// somewhere else". No such test existed: `SPG_TEMP_DIR` was one of the
/// twenty-seven switches the register listed as exercised by nothing.
///
/// It decides where a spilling sort writes, which is a disk an operator
/// chose — a deployer who points it at a data volume and is silently
/// ignored fills the wrong one.
pub fn temp_dir_from(raw: Option<std::ffi::OsString>) -> PathBuf {
    raw.filter(|v| !v.is_empty())
        .map_or_else(std::env::temp_dir, PathBuf::from)
}

/// How much a run buffers before it touches the file, in either
/// direction.
///
/// v7.37 (round 840) — a run is written and read one RECORD at a time: a
/// 4-byte length then a ~200-byte body, four syscalls per row. Against
/// an unbuffered `File` that cost 823 ms to write and 233 ms to read
/// 80 MB of 400k rows — about 97 MB/s, which is nothing to do with the
/// disk and everything to do with 1.6M syscalls. It is the whole of the
/// gap that kept the external merge sort from being wired (round 837
/// measured the spilled sort 5.4x slower than PG18; round 839 cleared
/// the codec and the merge at 186 ms combined).
///
/// 256 KiB is the usual plateau for this: large enough that per-record
/// syscalls disappear, small enough that a query with many runs open at
/// once does not pay for it in memory — and the sorter's own budget
/// bounds how many that is.
const RUN_BUF_BYTES: usize = 256 * 1024;

/// A single spill run backed by one file.
///
/// Buffered on both sides. The write buffer is flushed by `seal`, which
/// is also what rewinds the file, so a read can never see a partial
/// write.
pub struct FileRun {
    pub(crate) path: PathBuf,
    file: File,
    /// Pending bytes, either not yet written or already read ahead.
    buf: Vec<u8>,
    /// How far into `buf` the reader has got. Unused while writing.
    read_pos: usize,
    written: u64,
    sealed: bool,
}

impl TempRun for FileRun {
    fn append(&mut self, bytes: &[u8]) -> Result<(), TempStoreError> {
        debug_assert!(!self.sealed, "append after seal");
        self.buf.extend_from_slice(bytes);
        if self.buf.len() >= RUN_BUF_BYTES {
            self.file
                .write_all(&self.buf)
                .map_err(|e| io_err("writing spill run", &e))?;
            self.buf.clear();
        }
        self.written += bytes.len() as u64;
        Ok(())
    }

    fn seal(&mut self) -> Result<(), TempStoreError> {
        if !self.buf.is_empty() {
            self.file
                .write_all(&self.buf)
                .map_err(|e| io_err("writing spill run", &e))?;
            self.buf.clear();
        }
        self.read_pos = 0;
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
        if self.read_pos == self.buf.len() {
            self.buf.resize(RUN_BUF_BYTES, 0);
            let got = self
                .file
                .read(&mut self.buf)
                .map_err(|e| io_err("reading spill run", &e))?;
            self.buf.truncate(got);
            self.read_pos = 0;
            if got == 0 {
                return Ok(0);
            }
        }
        let n = core::cmp::min(buf.len(), self.buf.len() - self.read_pos);
        buf[..n].copy_from_slice(&self.buf[self.read_pos..self.read_pos + n]);
        self.read_pos += n;
        Ok(n)
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
    let rd = run_dir(dir);
    let _ = fs::create_dir_all(&rd);
    let path = rd.join(format!("{RUN_PREFIX}{}-{serial}.run", std::process::id()));
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
        buf: Vec::with_capacity(RUN_BUF_BYTES),
        read_pos: 0,
        written: 0,
        sealed: false,
    })
}

/// Remove run files this process cannot own — anything matching the run
/// prefix whose pid is not ours. Called once at startup; a `kill -9`
/// leaves files that no `Drop` will ever reach.
/// v7.38.19 — read SPG's OWN run directory, not the whole temp
/// directory.
///
/// This scanned everything in `$TMPDIR` at every server start, so the
/// cost of starting SPG was the size of a directory SPG does not own. On
/// the machine this was found on that directory held 61,708 entries and
/// 30 GB, and one `readdir` over it took **95 seconds** — so every
/// server start, including every one an e2e test spawns, waited a minute
/// and a half before it could listen. The failures that produces read
/// exactly like a busy machine: `EWOULDBLOCK` on a socket read, "server
/// didn't publish native listen addr within Ns".
///
/// The entries were `spg-e2e-*`, `spg-cli-*` and friends — this
/// project's own tests, which build a unique path per run under
/// `std::env::temp_dir()` and never remove it. `sweep_orphans` could
/// never have collected them: it only removes `spg-sort-*`. So the sweep
/// paid for reading tens of thousands of files it was not allowed to
/// touch.
///
/// Confining the run files to a subdirectory bounds the scan by what SPG
/// itself wrote, whatever else lives in the temp directory.
pub fn run_dir(base: &Path) -> PathBuf {
    base.join("spg-run")
}

pub fn sweep_orphans(dir: &Path) -> usize {
    let mine = format!("{RUN_PREFIX}{}-", std::process::id());
    let Ok(entries) = fs::read_dir(run_dir(dir)) else {
        return 0;
    };
    let mut removed = 0;
    for e in entries.flatten() {
        let name = e.file_name();
        let Some(name) = name.to_str() else { continue };
        if name.starts_with(RUN_PREFIX)
            && !name.starts_with(&mine)
            && fs::remove_file(e.path()).is_ok()
        {
            removed += 1;
        }
    }
    removed
}
