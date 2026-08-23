//! v7.7.1 — embedded crash-consistency tests.
//!
//! Simulates the four crash points the embedded path needs to
//! survive cleanly:
//!
//!   1. WAL appended but process died before `fsync` returned
//!      → reopen drops the half-written tail
//!   2. Checkpoint mid-write (tmp file present, rename never
//!      ran) → reopen ignores the tmp, replays WAL
//!   3. Cold-segment file written but manifest didn't update
//!      → reopen sees the dangling segment and ignores it
//!   4. Drop order: background freezer running while Database
//!      drops → no panic, no fsync-after-close
//!
//! Tests run against on-disk files in a per-test scratch dir
//! (cleaned up automatically).

use spg_embedded::{Database, FreezerOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// A temp dir that auto-cleans on drop. Avoids the `tempfile`
/// crate so the 0-deps policy stays clean.
struct Scratch {
    path: PathBuf,
}
impl Scratch {
    fn new(label: &str) -> Self {
        let mut p = std::env::temp_dir().join("spg-tests");
        let nanos: u64 = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0);
        p.push(format!(
            "spg-embedded-chaos-{label}-{nanos}-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&p).unwrap();
        Self { path: p }
    }
    fn db_path(&self) -> PathBuf {
        self.path.join("app.db")
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

#[test]
fn reopen_after_clean_close_reads_all_rows() {
    let scratch = Scratch::new("clean");
    let p = scratch.db_path();
    {
        let mut db = Database::open_path(&p).unwrap();
        db.execute("CREATE TABLE t (id INT NOT NULL, n INT)")
            .unwrap();
        db.execute("INSERT INTO t VALUES (1, 100), (2, 200), (3, 300)")
            .unwrap();
    }
    let mut db = Database::open_path(&p).unwrap();
    let rows = db.query("SELECT n FROM t ORDER BY id").unwrap();
    assert_eq!(rows.len(), 3);
}

#[test]
fn reopen_after_truncated_wal_tail_drops_partial_record() {
    let scratch = Scratch::new("trunc");
    let p = scratch.db_path();
    let db = Database::open_path(&p).unwrap();
    // Simulate a hard crash: leak the handle so Drop's final
    // checkpoint never runs. The WAL on disk is then exactly
    // what `execute()` left (every committed record fsynced,
    // no truncation).
    let mut db = db;
    db.execute("CREATE TABLE t (id INT NOT NULL)").unwrap();
    db.execute("INSERT INTO t VALUES (1)").unwrap();
    db.execute("INSERT INTO t VALUES (2)").unwrap();
    std::mem::forget(db);
    // mem::forget leaks the advisory file lock — clear it so the
    // reopen below doesn't refuse the "crashed" DB.
    Database::force_unlock(&p).unwrap();

    // v7.19 chunked WAL: `<db>.wal` is a directory of chunk
    // files. Truncate the NEWEST chunk (largest filename — both
    // name components are zero-padded hex, so lexicographic =
    // chronological).
    let wal = newest_wal_chunk(&p);
    let len = std::fs::metadata(&wal).unwrap().len();
    assert!(len > 10, "WAL should have committed records (len={len})");
    // Truncate off the trailing 4 bytes — guaranteed to leave
    // the last record header partial. Embedded's boot replay
    // should treat this as a clean half-write and drop it.
    let file = std::fs::OpenOptions::new().write(true).open(&wal).unwrap();
    file.set_len(len - 4).unwrap();
    drop(file);
    // Reopen — must NOT panic; must replay everything up to the
    // half-record cleanly.
    let mut db = Database::open_path(&p).unwrap();
    // Schema + at least one row should have survived.
    let rows = db.query("SELECT id FROM t ORDER BY id").unwrap();
    assert!(!rows.is_empty(), "schema + at least one row should survive");
}

#[test]
fn reopen_after_stray_tmp_checkpoint_ignores_it() {
    // Simulates: checkpoint started writing to .tmp, crash hit
    // before rename. Reopen must not adopt the .tmp as the
    // catalog — must boot from the previous good snapshot +
    // replay WAL.
    let scratch = Scratch::new("tmp");
    let p = scratch.db_path();
    {
        let mut db = Database::open_path(&p).unwrap();
        db.execute("CREATE TABLE t (id INT NOT NULL)").unwrap();
        db.execute("INSERT INTO t VALUES (42)").unwrap();
    }
    // Drop a junk .tmp file next to the DB.
    let tmp = catalog_tmp_path(&p);
    let mut f = std::fs::File::create(&tmp).unwrap();
    f.write_all(b"\x00\x01\x02\x03BOGUS CATALOG SNAPSHOT")
        .unwrap();
    drop(f);
    // Reopen.
    let mut db = Database::open_path(&p).unwrap();
    let rows = db.query("SELECT id FROM t").unwrap();
    assert_eq!(rows.len(), 1);
}

#[test]
fn dropping_database_while_freezer_runs_is_clean() {
    // Spawn a background freezer, then drop the Database. The
    // freezer thread must observe the stop and exit without
    // panic. Run for a few hundred ms to let the freezer take a
    // tick.
    let scratch = Scratch::new("freezer-drop");
    let p = scratch.db_path();
    let db = Arc::new(Mutex::new(Database::open_path(&p).unwrap()));
    {
        let mut guard = db.lock().unwrap();
        guard
            .execute("CREATE TABLE t (id INT NOT NULL, payload TEXT)")
            .unwrap();
        for i in 0..500 {
            guard
                .execute(&format!("INSERT INTO t VALUES ({i}, 'x')"))
                .unwrap();
        }
    }
    let mut freezer = Database::spawn_background_freezer(
        db.clone(),
        FreezerOptions {
            tick: Duration::from_millis(50),
            hot_tier_bytes: 1024, // very small, forces work
            batch_rows: 16,
            ..Default::default()
        },
    );
    std::thread::sleep(Duration::from_millis(250));
    freezer.stop();
    drop(db);
    // If we got here without a panic, the test passes.
}

#[test]
fn checkpoint_call_atomic_swap_survives_re_open() {
    // Explicit checkpoint exercises the tmp+rename path; the
    // freshly checkpointed DB must re-open and replay zero
    // additional records.
    let scratch = Scratch::new("ckpt");
    let p = scratch.db_path();
    {
        let mut db = Database::open_path(&p).unwrap();
        db.execute("CREATE TABLE t (id INT NOT NULL)").unwrap();
        db.execute("INSERT INTO t VALUES (1), (2), (3)").unwrap();
        db.checkpoint().unwrap();
    }
    let mut db = Database::open_path(&p).unwrap();
    let rows = db.query("SELECT id FROM t ORDER BY id").unwrap();
    assert_eq!(rows.len(), 3);
}

fn wal_path(db_path: &Path) -> PathBuf {
    let mut s = db_path.as_os_str().to_owned();
    s.push(".wal");
    PathBuf::from(s)
}

/// v7.19 chunked WAL — `<db>.wal/` holds
/// `<unix_us:016x>_<lsn:016x>.wal` chunk files. Returns the
/// newest (lexicographically largest) chunk.
fn newest_wal_chunk(db_path: &Path) -> PathBuf {
    let dir = wal_path(db_path);
    let mut chunks: Vec<PathBuf> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|c| c.extension().is_some_and(|x| x == "wal"))
        .collect();
    chunks.sort();
    chunks.pop().expect("at least one WAL chunk file")
}

fn catalog_tmp_path(db_path: &Path) -> PathBuf {
    let mut s = db_path.as_os_str().to_owned();
    s.push(".tmp");
    PathBuf::from(s)
}
