//! v7.1.4 — cold-segment manifest persistence + reload.
//!
//! Closes the last v7.1 carve-out. `Database::open_path(p)`
//! now boots with the same durability story as `spg-server`'s
//! manifest-driven cold-tier reload:
//!
//!   - `freeze_oldest_to_cold(...)` persists each segment to
//!     `<db>.spg/segments/seg_<id>.spg` (tmp+rename atomic);
//!   - `checkpoint()` (or `Drop`) writes
//!     `<db>.spg/manifest.v10` with one entry per active
//!     segment + CRC32;
//!   - next `open_path` verifies the manifest's `catalog_crc32`
//!     against the snapshot bytes, then each segment's CRC,
//!     then registers the segment with the engine at its
//!     original `segment_id` so BTree-Cold locators resolve
//!     byte-identically across the bounce.

use std::path::PathBuf;

use spg_embedded::Database;

fn unique_tmpdir(label: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let p = std::env::temp_dir()
        .join("spg-tests")
        .join(format!("spg-embed-cold-{label}-{nanos}"));
    std::fs::create_dir_all(&p).unwrap();
    p
}

#[test]
fn freeze_then_reopen_resolves_cold_pks() {
    let dir = unique_tmpdir("freeze");
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
        // Freeze rows 0..6 → cold segment 0.
        let report = db.freeze_oldest_to_cold("t", "by_id", 6).unwrap();
        assert_eq!(report.segment_id, 0);
        assert_eq!(report.frozen_rows, 6);
        // Explicit checkpoint so the manifest lands before drop.
        db.checkpoint().unwrap();
    }
    // Reopen: cold segment must come back via manifest, every
    // PK still resolvable via index seek.
    let mut db = Database::open_path(&db_path).unwrap();
    for id in 0..10i64 {
        let got = db
            .query(&format!("SELECT id FROM t WHERE id = {id}"))
            .unwrap();
        assert_eq!(got.len(), 1, "PK {id} lost across reopen");
    }
    // The reopened catalog should still know about exactly one
    // cold segment.
    assert_eq!(
        db.engine().catalog().cold_segment_count(),
        1,
        "expected exactly 1 cold segment after reload"
    );
}

#[test]
fn manifest_persists_across_drop_without_explicit_checkpoint() {
    let dir = unique_tmpdir("drop");
    let db_path = dir.join("spg.db");
    {
        let mut db = Database::open_path(&db_path).unwrap();
        db.execute("CREATE TABLE t (id INT NOT NULL)").unwrap();
        db.execute("CREATE INDEX by_id ON t (id)").unwrap();
        for id in 0..5i64 {
            db.execute(&format!("INSERT INTO t VALUES ({id})")).unwrap();
        }
        db.freeze_oldest_to_cold("t", "by_id", 3).unwrap();
        // No explicit checkpoint — rely on Drop.
    }
    let mut db = Database::open_path(&db_path).unwrap();
    // All 5 PKs must resolve. v6.x cold-tier is reached only
    // via index seek, not via full-table scan (`SELECT
    // count(*)` would skip the 3 frozen rows), so we probe by
    // PK explicitly.
    let mut hits = 0;
    for id in 0..5i64 {
        let got = db
            .query(&format!("SELECT id FROM t WHERE id = {id}"))
            .unwrap();
        if !got.is_empty() {
            hits += 1;
        }
    }
    assert_eq!(hits, 5, "all 5 PKs must resolve after reopen");
}

#[test]
fn manifest_files_are_in_expected_locations() {
    let dir = unique_tmpdir("layout");
    let db_path = dir.join("spg.db");
    let mut db = Database::open_path(&db_path).unwrap();
    db.execute("CREATE TABLE t (id INT NOT NULL)").unwrap();
    db.execute("CREATE INDEX by_id ON t (id)").unwrap();
    db.execute("INSERT INTO t VALUES (1)").unwrap();
    db.execute("INSERT INTO t VALUES (2)").unwrap();
    db.freeze_oldest_to_cold("t", "by_id", 2).unwrap();
    db.checkpoint().unwrap();
    // Layout sanity matches `spg-server`:
    //   <dir>/spg.db                   — catalog snapshot
    //   <dir>/spg.db.wal               — write-ahead log
    //   <dir>/spg.spg/segments/seg_0.spg  — cold segment
    //   <dir>/spg.spg/manifest.v10        — manifest
    let segments_dir = dir.join("spg.spg/segments");
    let seg_file = segments_dir.join("seg_0.spg");
    let manifest_file = dir.join("spg.spg/manifest.v10");
    assert!(seg_file.exists(), "cold segment file at {seg_file:?}");
    assert!(manifest_file.exists(), "manifest at {manifest_file:?}");
    assert!(db_path.exists());
}
