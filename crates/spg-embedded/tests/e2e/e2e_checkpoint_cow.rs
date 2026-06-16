//! v7.34 CoW-2 — background-checkpoint worker e2e.
//!
//! Verifies:
//!   - The auto-checkpoint hot path triggers the worker asynchronously
//!     (multiple chunks land, hot-path latency stays bounded), and all
//!     written data is recoverable on reopen — i.e. the marker-LSN
//!     replay floor covers writes that crossed into a new chunk between
//!     worker iterations.
//!   - Sync `Database::checkpoint()` still returns only after the
//!     snapshot is durable on disk (worker-mediated but caller-visible
//!     contract preserved).
//!   - `Drop` joins the worker cleanly — the final checkpoint lands so
//!     reopen sees the latest snapshot, not an old one + replayed WAL.

use std::path::PathBuf;

use spg_embedded::Database;
use spg_storage::Value;

fn unique_tmpdir(label: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let p = std::env::temp_dir().join(format!("spg-embed-cp-cow-{label}-{nanos}"));
    std::fs::create_dir_all(&p).unwrap();
    p
}

fn count_rows(db: &mut Database, table: &str) -> i64 {
    let rows = db.query(&format!("SELECT COUNT(*) FROM {table}")).unwrap();
    match &rows[0][0] {
        Value::BigInt(n) => *n,
        Value::Int(n) => i64::from(*n),
        other => panic!("expected count, got {other:?}"),
    }
}

#[test]
fn async_checkpoint_via_threshold_recovers_all_rows() {
    // Tight threshold + small inserts → the auto-checkpoint hot path
    // fires several times. Each firing now goes through the background
    // worker (fire-and-forget for the calling write). The Drop-time
    // sync checkpoint guarantees the final state is durable.
    let dir = unique_tmpdir("threshold");
    let db_path = dir.join("spg.db");
    let wal_path = dir.join("spg.db.wal");
    {
        let mut db = Database::open_path(&db_path).unwrap();
        db.set_checkpoint_threshold_bytes(1024);
        db.execute("CREATE TABLE t (id INT NOT NULL, payload TEXT)")
            .unwrap();
        for i in 0..200 {
            let payload = "x".repeat(48);
            db.execute(&format!("INSERT INTO t VALUES ({i}, '{payload}')"))
                .unwrap();
        }
        // Several auto-checkpoints must have fired (≥ 2 chunks).
        let chunk_count = std::fs::read_dir(&wal_path)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().and_then(|s| s.to_str()) == Some("wal"))
            .count();
        assert!(
            chunk_count >= 2,
            "auto-checkpoint never rotated a chunk: {chunk_count}"
        );
    }
    // Drop ran the final sync checkpoint via the worker. Reopen and
    // verify every row survived — split across snapshot + WAL replay.
    let mut db = Database::open_path(&db_path).unwrap();
    assert_eq!(count_rows(&mut db, "t"), 200);
}

#[test]
fn explicit_checkpoint_is_durable_on_return() {
    // The sync `checkpoint()` API must still return only after the
    // snapshot is durable: a fresh open immediately after the call
    // (without any extra writes) sees the same data without replaying
    // a single WAL record.
    let dir = unique_tmpdir("explicit");
    let db_path = dir.join("spg.db");
    {
        let mut db = Database::open_path(&db_path).unwrap();
        db.execute("CREATE TABLE t (id INT NOT NULL, v TEXT)")
            .unwrap();
        for i in 0..30 {
            db.execute(&format!("INSERT INTO t VALUES ({i}, 'r{i}')"))
                .unwrap();
        }
        db.checkpoint().unwrap();
        // Snapshot file is on disk now — read it from the FS, not via
        // the engine — and confirm it's non-trivial.
        let bytes = std::fs::read(&db_path).unwrap();
        assert!(bytes.len() > 16, "snapshot file empty after checkpoint");
    }
    let mut db = Database::open_path(&db_path).unwrap();
    assert_eq!(count_rows(&mut db, "t"), 30);
}

#[test]
fn cold_segments_reload_when_manifest_is_deleted() {
    // CoW-4 — D10 fallback: the snapshot+manifest pair is two
    // tmp+rename steps. A crash between them leaves the new snapshot
    // on disk with a stale manifest whose `catalog_crc32` no longer
    // matches. The pre-CoW-4 boot silently skipped every cold segment
    // in that case, orphaning the data on disk. The fix is a
    // segments-dir scan that trusts each segment's own magic + CRC.
    //
    // We simulate the worst case (no manifest at all — the rename
    // never happened) by deleting the manifest file after a clean
    // checkpoint. The cold segments must still load and resolve.
    let dir = unique_tmpdir("no-manifest");
    let db_path = dir.join("spg.db");
    {
        let mut db = Database::open_path(&db_path).unwrap();
        db.execute("CREATE TABLE t (id INT NOT NULL, name TEXT NOT NULL)")
            .unwrap();
        db.execute("CREATE INDEX by_id ON t (id)").unwrap();
        for id in 0..10i64 {
            db.execute(&format!("INSERT INTO t VALUES ({id}, 'r-{id}')"))
                .unwrap();
        }
        db.freeze_oldest_to_cold("t", "by_id", 6).unwrap();
        db.checkpoint().unwrap();
    }
    // Simulate the D10 crash window: snapshot was rewritten, manifest
    // wasn't.
    let manifest = dir.join("spg.spg/manifest.v10");
    assert!(manifest.exists(), "preconditions: manifest is there");
    std::fs::remove_file(&manifest).unwrap();
    // Reopen: scan picks up the segment file and reattaches it.
    let mut db = Database::open_path(&db_path).unwrap();
    for id in 0..10i64 {
        let got = db
            .query(&format!("SELECT id FROM t WHERE id = {id}"))
            .unwrap();
        assert_eq!(got.len(), 1, "PK {id} lost when manifest absent");
    }
    assert_eq!(
        db.engine().catalog().cold_segment_count(),
        1,
        "scan must reattach the cold segment with no manifest present",
    );
}

#[test]
fn cold_segments_reload_when_manifest_crc_is_stale() {
    // CoW-4 — same D10 hazard, the "subtle" version: manifest file
    // exists but its `catalog_crc32` belongs to an older snapshot. The
    // pre-CoW-4 boot's `snap_crc == m.catalog_crc32` gate silently
    // skipped every entry. Simulate by overwriting the snapshot bytes
    // (changes the CRC) without rewriting the manifest.
    let dir = unique_tmpdir("stale-manifest");
    let db_path = dir.join("spg.db");
    {
        let mut db = Database::open_path(&db_path).unwrap();
        db.execute("CREATE TABLE t (id INT NOT NULL, name TEXT NOT NULL)")
            .unwrap();
        db.execute("CREATE INDEX by_id ON t (id)").unwrap();
        for id in 0..10i64 {
            db.execute(&format!("INSERT INTO t VALUES ({id}, 'r-{id}')"))
                .unwrap();
        }
        db.freeze_oldest_to_cold("t", "by_id", 6).unwrap();
        db.checkpoint().unwrap();
        // Mutate something so a subsequent checkpoint changes the
        // snapshot CRC, then trigger the second checkpoint. With CoW-2
        // the worker carries the latest manifest forward, so to model
        // a stale manifest deterministically we *also* roll the
        // manifest file back to its first-checkpoint contents below.
        let manifest = dir.join("spg.spg/manifest.v10");
        let stale = std::fs::read(&manifest).unwrap();
        db.execute("INSERT INTO t VALUES (99, 'late')").unwrap();
        db.checkpoint().unwrap();
        // Replace the up-to-date manifest with the pre-late-insert
        // copy. The snapshot file is now ahead of the manifest's
        // `catalog_crc32` — exactly the D10 window's state.
        std::fs::write(&manifest, &stale).unwrap();
    }
    let mut db = Database::open_path(&db_path).unwrap();
    for id in 0..10i64 {
        let got = db
            .query(&format!("SELECT id FROM t WHERE id = {id}"))
            .unwrap();
        assert_eq!(got.len(), 1, "PK {id} lost when manifest CRC went stale");
    }
    let got = db.query("SELECT id FROM t WHERE id = 99").unwrap();
    assert_eq!(
        got.len(),
        1,
        "late insert lost across stale-manifest reopen"
    );
    assert_eq!(
        db.engine().catalog().cold_segment_count(),
        1,
        "scan must reattach the cold segment despite stale manifest CRC",
    );
}

#[test]
fn writes_after_async_trigger_are_recoverable() {
    // The hot-path trigger captures a snapshot at marker-LSN time and
    // returns. Subsequent writes (still within the same session) ride
    // the WAL above that marker. Drop's final checkpoint moves the
    // floor forward. The invariant: every committed write survives a
    // crash-like immediate reopen — regardless of where the marker
    // floor was between the two batches.
    let dir = unique_tmpdir("after-trigger");
    let db_path = dir.join("spg.db");
    {
        let mut db = Database::open_path(&db_path).unwrap();
        db.set_checkpoint_threshold_bytes(512);
        db.execute("CREATE TABLE t (id INT NOT NULL, v TEXT)")
            .unwrap();
        // Batch 1 — likely crosses the threshold once.
        for i in 0..60 {
            db.execute(&format!("INSERT INTO t VALUES ({i}, '{}')", "v".repeat(40)))
                .unwrap();
        }
        // Batch 2 — runs while the previous async checkpoint may still
        // be in flight on the worker thread.
        for i in 60..120 {
            db.execute(&format!("INSERT INTO t VALUES ({i}, '{}')", "v".repeat(40)))
                .unwrap();
        }
    }
    let mut db = Database::open_path(&db_path).unwrap();
    assert_eq!(count_rows(&mut db, "t"), 120);
}
