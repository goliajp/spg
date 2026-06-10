//! v7.18 PITR P1 — WAL record schema v4 (commit_lsn + commit_unix_us).
//!
//! Covers the wire-level invariants the rest of the PITR sub-epic
//! depends on:
//!
//! * v4 records round-trip through `parse_wal_records` carrying
//!   their LSN + timestamp.
//! * A WAL containing only v3 records still loads (backward compat).
//! * A WAL with mixed v3 + v4 records replays cleanly — the loop
//!   uses the same envelope flag bits and only switches on the
//!   per-record type byte.
//! * Reopening a file-backed Database recovers the commit_lsn
//!   counter from the WAL high-water mark, so successive sessions
//!   never re-issue an LSN.

use spg_embedded::{Database, parse_wal_records};
use std::path::{Path, PathBuf};
use tempfile::TempDir;

/// v7.19 — concatenate every chunk under `<db_path>.wal/` in
/// sorted (= lexicographic = chunk-creation) order. The v7.18
/// PITR tests assumed a single `<db_path>.wal` file; this
/// helper rebuilds the same byte stream for assertions that
/// `parse_wal_records` runs across the whole post-snapshot
/// history.
fn read_wal_chunks(db_path: &Path) -> Vec<u8> {
    let mut wal_dir = PathBuf::from(db_path);
    let mut name = wal_dir
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_default();
    name.push(".wal");
    wal_dir.set_file_name(name);
    let mut entries: Vec<PathBuf> = match std::fs::read_dir(&wal_dir) {
        Ok(rd) => rd
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("wal"))
            .collect(),
        Err(_) => return Vec::new(),
    };
    entries.sort();
    let mut out = Vec::new();
    for p in entries {
        if let Ok(b) = std::fs::read(&p) {
            out.extend_from_slice(&b);
        }
    }
    out
}

#[test]
fn v4_records_round_trip_lsn_and_timestamp() {
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("v4.db");
    let wal_path = tmp.path().join("v4.db.wal");

    {
        let mut db = Database::open_path(&db_path).unwrap();
        db.execute("CREATE TABLE t (id INT NOT NULL)").unwrap();
        db.execute("INSERT INTO t VALUES (1)").unwrap();
        db.execute("INSERT INTO t VALUES (2)").unwrap();
    } // Drop -> final checkpoint.

    // The Drop checkpoint truncates the WAL. To inspect v4 records
    // mid-flight, open a fresh DB and stop short of Drop.
    let mut db = Database::open_path(&db_path).unwrap();
    db.execute("CREATE TABLE u (id INT NOT NULL)").unwrap();
    db.execute("INSERT INTO u VALUES (10)").unwrap();
    let wal_bytes = read_wal_chunks(&db_path);
    let records = parse_wal_records(&wal_bytes).unwrap();

    assert!(
        records.len() >= 2,
        "expected at least 2 records, got {}",
        records.len()
    );
    // Every SQL record produced by v7.18+ spg-embedded is v4
    // auto-commit (type 0x10) with an LSN + timestamp.
    // v7.19 also emits checkpoint markers (type 0x11) into the
    // chunk before rotation; skip them — they too carry LSN/ts
    // but their `sql` is the snapshot path, not user SQL.
    let sql_records: Vec<_> = records.iter().filter(|r| r.type_byte == 0x10).collect();
    assert!(
        !sql_records.is_empty(),
        "expected at least 1 SQL record, got {} markers",
        records.len()
    );
    for r in &sql_records {
        assert!(r.commit_lsn.is_some(), "v4 record must carry LSN");
        assert!(r.commit_unix_us.is_some(), "v4 record must carry timestamp");
    }
    // LSNs strictly monotonic across SQL + marker records.
    let lsns: Vec<u64> = records.iter().filter_map(|r| r.commit_lsn).collect();
    for w in lsns.windows(2) {
        assert!(w[0] <= w[1], "LSN must be non-decreasing: {lsns:?}");
    }
}

#[test]
fn reopen_recovers_lsn_watermark() {
    // Two file-backed sessions: after the first, the second
    // session's first write must NOT reuse the LSN the first
    // session's final write claimed. We don't have a direct
    // pub accessor for commit_lsn, so we read the WAL between
    // sessions and assert the second session's records start
    // above the first session's max.
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("ws.db");
    let wal_path = tmp.path().join("ws.db.wal");

    let first_max_lsn = {
        let mut db = Database::open_path(&db_path).unwrap();
        db.execute("CREATE TABLE t (id INT NOT NULL)").unwrap();
        db.execute("INSERT INTO t VALUES (1)").unwrap();
        db.execute("INSERT INTO t VALUES (2)").unwrap();
        // Skip Drop's checkpoint to keep the WAL alive — mem::forget
        // also leaks the file lock, so we force_unlock below to let
        // a second session open the same path.
        let bytes = read_wal_chunks(&db_path);
        let recs = parse_wal_records(&bytes).unwrap();
        let max = recs.iter().filter_map(|r| r.commit_lsn).max().unwrap();
        std::mem::forget(db);
        max
    };
    Database::force_unlock(&db_path).unwrap();

    // Second session reopens; first write must use LSN > first_max_lsn.
    let mut db = Database::open_path(&db_path).unwrap();
    db.execute("INSERT INTO t VALUES (3)").unwrap();
    let bytes = read_wal_chunks(&db_path);
    let recs = parse_wal_records(&bytes).unwrap();
    let post_max = recs.iter().filter_map(|r| r.commit_lsn).max().unwrap();
    assert!(
        post_max > first_max_lsn,
        "second-session max LSN {post_max} must exceed first-session max {first_max_lsn}"
    );
    std::mem::forget(db);
    Database::force_unlock(&db_path).ok();
}

#[test]
fn v3_records_still_load_for_backward_compat() {
    // Hand-write a synthetic v3-only WAL (the encode_v3 function
    // is pub(crate) — we mimic its layout here). Then open the
    // database and confirm replay applies the statement.
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("v3.db");
    let wal_path = tmp.path().join("v3.db.wal");

    let sql = "CREATE TABLE t (a INT NOT NULL)";
    let payload = sql.as_bytes();
    const WAL_V2_SENTINEL: u32 = 0x8000_0000;
    const WAL_V3_FLAG: u32 = 0x4000_0000;
    const WAL_V3_TYPE_AUTO_COMMIT_SQL: u8 = 0x01;
    let mut crc_buf = Vec::with_capacity(1 + payload.len());
    crc_buf.push(WAL_V3_TYPE_AUTO_COMMIT_SQL);
    crc_buf.extend_from_slice(payload);
    let crc = spg_crypto::crc32::crc32(&crc_buf);
    let header = ((payload.len() as u32) | WAL_V2_SENTINEL | WAL_V3_FLAG).to_le_bytes();
    let mut wal = Vec::new();
    wal.extend_from_slice(&header);
    wal.extend_from_slice(&crc.to_le_bytes());
    wal.push(WAL_V3_TYPE_AUTO_COMMIT_SQL);
    wal.extend_from_slice(payload);
    std::fs::write(&wal_path, &wal).unwrap();

    let mut db = Database::open_path(&db_path).unwrap();
    db.execute("INSERT INTO t VALUES (1)").unwrap();
    let rows = db.query("SELECT COUNT(*) FROM t").unwrap();
    assert_eq!(rows.len(), 1);
    std::mem::forget(db);

    // Inspect: the WAL now contains the v3 prefix (migrated by
    // open_path into wal_dir/0000…wal) + a v4 record from the
    // INSERT we just did. read_wal_chunks() concats every chunk
    // in sorted order so both records show up.
    let bytes = read_wal_chunks(&db_path);
    let recs = parse_wal_records(&bytes).unwrap();
    assert!(recs.iter().any(|r| r.type_byte == 0x01), "v3 record");
    assert!(recs.iter().any(|r| r.type_byte == 0x10), "v4 record");
}

#[test]
fn checkpoint_marker_parses_back_to_lsn_ts_path() {
    // The encode_v4_checkpoint_marker helper isn't pub, but the
    // checkpoint() call inside Database emits one before WAL
    // truncate. Since the truncate erases it from disk, we
    // exercise the marker by hand-crafting a WAL chunk: a v4 SQL
    // record + a marker record, then parse both back and assert
    // the marker exposes lsn / ts / snapshot_path.
    let tmp = TempDir::new().unwrap();
    let wal_path = tmp.path().join("synthetic.wal");

    // Hand-write a v4 auto-commit record + v4 checkpoint marker.
    // Re-derive the marker layout from the docs so this test
    // catches accidental layout changes in the encoder.
    const WAL_V2_SENTINEL: u32 = 0x8000_0000;
    const WAL_V3_FLAG: u32 = 0x4000_0000;
    const WAL_V4_TYPE_AUTO_COMMIT_SQL: u8 = 0x10;
    const WAL_V4_TYPE_CHECKPOINT_MARKER: u8 = 0x11;

    // 1) v4 auto-commit record carrying lsn=7, ts=100.
    let sql = b"CREATE TABLE t (a INT)";
    let mut payload_buf = Vec::new();
    payload_buf.push(WAL_V4_TYPE_AUTO_COMMIT_SQL);
    payload_buf.extend_from_slice(&7u64.to_le_bytes());
    payload_buf.extend_from_slice(&100i64.to_le_bytes());
    payload_buf.extend_from_slice(sql);
    let crc = spg_crypto::crc32::crc32(&payload_buf);
    let mut wal = Vec::new();
    wal.extend_from_slice(&((sql.len() as u32) | WAL_V2_SENTINEL | WAL_V3_FLAG).to_le_bytes());
    wal.extend_from_slice(&crc.to_le_bytes());
    wal.push(WAL_V4_TYPE_AUTO_COMMIT_SQL);
    wal.extend_from_slice(&7u64.to_le_bytes());
    wal.extend_from_slice(&100i64.to_le_bytes());
    wal.extend_from_slice(sql);

    // 2) v4 checkpoint marker carrying lsn=8, ts=200, path=/tmp/x.spg.
    let snap_path = "/tmp/x.spg";
    let mut m_payload = Vec::new();
    m_payload.extend_from_slice(&8u64.to_le_bytes());
    m_payload.extend_from_slice(&200i64.to_le_bytes());
    m_payload.extend_from_slice(&(snap_path.len() as u16).to_le_bytes());
    m_payload.extend_from_slice(snap_path.as_bytes());
    let mut m_crc_buf = Vec::with_capacity(1 + m_payload.len());
    m_crc_buf.push(WAL_V4_TYPE_CHECKPOINT_MARKER);
    m_crc_buf.extend_from_slice(&m_payload);
    let m_crc = spg_crypto::crc32::crc32(&m_crc_buf);
    let m_payload_len = m_payload.len() as u32;
    wal.extend_from_slice(&(m_payload_len | WAL_V2_SENTINEL | WAL_V3_FLAG).to_le_bytes());
    wal.extend_from_slice(&m_crc.to_le_bytes());
    wal.push(WAL_V4_TYPE_CHECKPOINT_MARKER);
    wal.extend_from_slice(&m_payload);

    std::fs::write(&wal_path, &wal).unwrap();
    let bytes = std::fs::read(&wal_path).unwrap();
    let recs = parse_wal_records(&bytes).unwrap();
    assert_eq!(recs.len(), 2);

    // First record is the SQL.
    assert_eq!(recs[0].type_byte, WAL_V4_TYPE_AUTO_COMMIT_SQL);
    assert_eq!(recs[0].commit_lsn, Some(7));
    assert_eq!(recs[0].commit_unix_us, Some(100));
    assert_eq!(recs[0].sql, sql);

    // Second is the marker — sql field carries the snapshot path bytes.
    assert_eq!(recs[1].type_byte, WAL_V4_TYPE_CHECKPOINT_MARKER);
    assert_eq!(recs[1].commit_lsn, Some(8));
    assert_eq!(recs[1].commit_unix_us, Some(200));
    assert_eq!(recs[1].sql, snap_path.as_bytes());
}

#[test]
fn mixed_v3_v4_wal_replays_in_order() {
    // Same setup as the previous test but assert the engine sees
    // both statements applied — i.e. the v3 record applies first,
    // then the v4 record.
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("mix.db");
    let wal_path = tmp.path().join("mix.db.wal");

    // Hand-craft a v3 record for CREATE TABLE.
    let sql_v3 = "CREATE TABLE t (a INT NOT NULL)";
    let payload = sql_v3.as_bytes();
    const WAL_V2_SENTINEL: u32 = 0x8000_0000;
    const WAL_V3_FLAG: u32 = 0x4000_0000;
    const WAL_V3_TYPE_AUTO_COMMIT_SQL: u8 = 0x01;
    let mut crc_buf = Vec::with_capacity(1 + payload.len());
    crc_buf.push(WAL_V3_TYPE_AUTO_COMMIT_SQL);
    crc_buf.extend_from_slice(payload);
    let crc = spg_crypto::crc32::crc32(&crc_buf);
    let header = ((payload.len() as u32) | WAL_V2_SENTINEL | WAL_V3_FLAG).to_le_bytes();
    let mut wal = Vec::new();
    wal.extend_from_slice(&header);
    wal.extend_from_slice(&crc.to_le_bytes());
    wal.push(WAL_V3_TYPE_AUTO_COMMIT_SQL);
    wal.extend_from_slice(payload);
    std::fs::write(&wal_path, &wal).unwrap();

    // Now open + INSERT — drops v4 records on top.
    let mut db = Database::open_path(&db_path).unwrap();
    db.execute("INSERT INTO t VALUES (1)").unwrap();
    db.execute("INSERT INTO t VALUES (2)").unwrap();
    std::mem::forget(db);
    Database::force_unlock(&db_path).unwrap();

    // Reopen and confirm both rows survive — proves the v3
    // CREATE TABLE + v4 INSERTs replayed cleanly together.
    let mut db = Database::open_path(&db_path).unwrap();
    let r = db.query("SELECT COUNT(*) FROM t").unwrap();
    let count = match &r[0][0] {
        spg_embedded::Value::Int(n) => i64::from(*n),
        spg_embedded::Value::BigInt(n) => *n,
        other => panic!("unexpected: {other:?}"),
    };
    assert_eq!(count, 2);
    std::mem::forget(db);
    Database::force_unlock(&db_path).ok();
}
