//! mailrs embed round-14 — TEXT values > 64 KiB through every
//! durability path. The 7.22 storage codec panicked
//! ("identifier / text fits in u16") at the FIRST snapshot encode
//! after a big-body INSERT was already acknowledged; the freezer
//! additionally refused rows wider than a segment page, so big
//! mail could never reach the cold tier. Note:
//! `.claude/notes/mailrs-embed-round14-u16-text-panic.md`.

use spg_embedded::{Database, Value};
use std::path::PathBuf;

fn tmpdir(label: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let p = std::env::temp_dir().join(format!("spg-r14-{label}-{nanos}"));
    std::fs::create_dir_all(&p).unwrap();
    p
}

fn big_body(len: usize) -> String {
    "x".repeat(len)
}

/// The verbatim round-14 repro: INSERT a 70 KB body, graceful close
/// (Drop = final snapshot encode — the 7.22 panic site), reopen.
#[test]
fn big_text_survives_snapshot_close_and_reopen() {
    let dir = tmpdir("snapshot");
    let db_path = dir.join("t.spg");
    {
        let mut db = Database::open_path(&db_path).unwrap();
        db.execute("CREATE TABLE t (id BIGSERIAL PRIMARY KEY, body TEXT NOT NULL)")
            .unwrap();
        db.execute(&format!(
            "INSERT INTO t (body) VALUES ('{}')",
            big_body(70_000)
        ))
        .unwrap();
    } // Drop → snapshot encode: panicked on 7.22
    let mut db = Database::open_path(&db_path).unwrap();
    let rows = db.query("SELECT body FROM t").unwrap();
    match &rows[0][0] {
        Value::Text(s) => assert_eq!(s.len(), 70_000),
        other => panic!("expected Text, got {other:?}"),
    }
}

/// Crash shape (no Drop): the acknowledged INSERT must come back
/// from WAL replay — and the post-replay snapshot encode (the next
/// checkpoint) must also take the big row.
#[test]
fn big_text_survives_crash_and_wal_replay() {
    let dir = tmpdir("crash");
    let db_path = dir.join("t.spg");
    {
        let mut db = Database::open_path(&db_path).unwrap();
        db.execute("CREATE TABLE t (id BIGSERIAL PRIMARY KEY, body TEXT NOT NULL)")
            .unwrap();
        db.execute(&format!(
            "INSERT INTO t (body) VALUES ('{}')",
            big_body(1_048_576)
        ))
        .unwrap();
        std::mem::forget(db); // SIGKILL shape — exactly the 7.22
        // victim state: WAL has the row, no
        // snapshot was ever written.
    }
    Database::force_unlock(&db_path).unwrap();
    let mut db = Database::open_path(&db_path).unwrap();
    let rows = db.query("SELECT id FROM t").unwrap();
    assert_eq!(rows.len(), 1, "WAL replay must restore the big row");
    // And the checkpoint that 7.22 would have panicked on:
    db.checkpoint().unwrap();
}

/// Cold tier: freeze a table whose rows exceed the segment page
/// size, then read them back through the cold lookup path. 7.22's
/// freezer rejected such rows ("doesn't fit in page"), so big mail
/// pinned the hot tier forever.
#[test]
fn big_text_freezes_to_cold_tier_and_reads_back() {
    let dir = tmpdir("freeze");
    let db_path = dir.join("t.spg");
    let mut db = Database::open_path(&db_path).unwrap();
    db.execute("CREATE TABLE mail (id BIGINT NOT NULL, body TEXT NOT NULL)")
        .unwrap();
    for i in 0..4 {
        db.execute(&format!(
            "INSERT INTO mail VALUES ({i}, '{}')",
            big_body(200_000)
        ))
        .unwrap();
    }
    db.execute("CREATE INDEX mail_by_id ON mail (id)").unwrap();
    let report = db.freeze_oldest_to_cold("mail", "mail_by_id", 3).unwrap();
    assert_eq!(report.frozen_rows, 3, "jumbo rows must freeze");
    // Cold-tier contract: full scans see the hot tier; cold rows
    // surface via PK/index lookups (see e2e_freeze_visible). Every
    // frozen jumbo row must come back through the cold lookup path.
    for i in 0..3 {
        let rows = db
            .query(&format!("SELECT body FROM mail WHERE id = {i}"))
            .unwrap();
        assert_eq!(rows.len(), 1, "cold jumbo row {i} surfaces via PK");
        match &rows[0][0] {
            Value::Text(s) => assert_eq!(s.len(), 200_000, "row {i} intact"),
            other => panic!("expected Text, got {other:?}"),
        }
    }
}

/// The import path (what the prod cutover actually runs): a dump
/// shape with a > 64 KiB body in a COPY block and one in an INSERT.
#[test]
fn big_text_imports_via_execute_script() {
    let mut db = Database::open_in_memory();
    let body = big_body(80_000);
    let script = format!(
        "CREATE TABLE m (id bigint, body text);\n\
         COPY m (id, body) FROM stdin;\n\
         1\t{body}\n\
         \\.\n\
         INSERT INTO m VALUES (2, '{body}');\n"
    );
    db.execute_script(&script).unwrap();
    let rows = db.query("SELECT body FROM m ORDER BY id").unwrap();
    assert_eq!(rows.len(), 2);
    for r in &rows {
        match &r[0] {
            Value::Text(s) => assert_eq!(s.len(), 80_000),
            other => panic!("expected Text, got {other:?}"),
        }
    }
}
