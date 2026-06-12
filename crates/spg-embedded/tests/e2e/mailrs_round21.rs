//! mailrs embed round-21 — the BYTEA twin of round-14 (fired during
//! a PRODUCTION migration window) + namespace-aware lock liveness.
//! Note: `.claude/notes/mailrs-embed-round21-bytea-u16-and-container-lock.md`.

use spg_embedded::{Database, Value};
use std::path::PathBuf;

fn tmpdir(label: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let p = std::env::temp_dir().join(format!("spg-r21-{label}-{nanos}"));
    std::fs::create_dir_all(&p).unwrap();
    p
}

/// The verbatim migration shape: ALTER … TYPE BYTEA USING
/// decode(base64) over rows whose decoded payload exceeds 64 KiB,
/// then the snapshot encode that panicked on 7.26.
#[test]
fn big_bytea_survives_migration_and_snapshot() {
    let dir = tmpdir("alter");
    let db_path = dir.join("t.spg");
    {
        let mut db = Database::open_path(&db_path).unwrap();
        db.execute("CREATE TABLE outbound_queue (id BIGSERIAL PRIMARY KEY, message_data TEXT)")
            .unwrap();
        // ~90 KB of base64 (matches the round-21 repro).
        let b64 = "QUFB".repeat(30_000);
        db.execute(&format!(
            "INSERT INTO outbound_queue (message_data) VALUES ('{b64}')"
        ))
        .unwrap();
        db.execute(
            "ALTER TABLE outbound_queue ALTER COLUMN message_data TYPE BYTEA \
             USING decode(message_data, 'base64')",
        )
        .unwrap();
    } // Drop → snapshot encode: panicked on 7.26 ("BYTEA cell ≤ 64 KiB")
    let mut db = Database::open_path(&db_path).unwrap();
    let rows = db.query("SELECT message_data FROM outbound_queue").unwrap();
    match &rows[0][0] {
        Value::Bytes(b) => assert_eq!(b.len(), 90_000),
        other => panic!("expected Bytes, got {other:?}"),
    }
}

/// Live-path twin: big BYTEA through WAL crash replay + checkpoint,
/// and a > 64 KiB TEXT[] element alongside (the other u16 cell the
/// sweep closed).
#[test]
fn big_bytea_and_text_array_survive_crash_replay() {
    let dir = tmpdir("crash");
    let db_path = dir.join("t.spg");
    {
        let mut db = Database::open_path(&db_path).unwrap();
        db.execute("CREATE TABLE t (id BIGINT, data BYTEA, uris TEXT[])")
            .unwrap();
        let b64 = "QUFB".repeat(40_000); // 120 KB decoded
        let big_uri = "u".repeat(80_000);
        db.execute(&format!(
            "INSERT INTO t VALUES (1, decode('{b64}', 'base64'), ARRAY['{big_uri}', 'small'])"
        ))
        .unwrap();
        std::mem::forget(db);
    }
    Database::force_unlock(&db_path).unwrap();
    let mut db = Database::open_path(&db_path).unwrap();
    let rows = db.query("SELECT data, uris FROM t").unwrap();
    match &rows[0][0] {
        Value::Bytes(b) => assert_eq!(b.len(), 120_000),
        other => panic!("expected Bytes, got {other:?}"),
    }
    match &rows[0][1] {
        Value::TextArray(items) => assert_eq!(items[0].as_ref().unwrap().len(), 80_000),
        other => panic!("expected TextArray, got {other:?}"),
    }
    db.checkpoint().unwrap();
}

/// Round-21 B — a lock recorded in a DIFFERENT host/container must
/// report UNDECIDABLE (with a force_unlock pointer), not "locked by
/// pid 1" (the prober's own init is always alive).
#[test]
fn foreign_namespace_lock_reports_undecidable() {
    let dir = tmpdir("lock");
    let db_path = dir.join("t.spg");
    {
        let mut db = Database::open_path(&db_path).unwrap();
        db.execute("CREATE TABLE t (n BIGINT)").unwrap();
    }
    // Forge the lock as container A would have left it: pid 1,
    // hostname that is not ours.
    let lock = dir.join("t.spg.lock");
    std::fs::create_dir(&lock).unwrap();
    std::fs::write(lock.join("pid"), "1\ncontainer-aaaa\nboot-xyz\n").unwrap();
    let err = Database::open_path(&db_path).unwrap_err().to_string();
    assert!(err.contains("different host/container"), "{err}");
    assert!(err.contains("force_unlock"), "{err}");
    assert!(
        !err.contains("locked by another process (pid 1)"),
        "must not misreport pid-1 liveness: {err}"
    );
    // force_unlock is the documented recovery — and it works.
    Database::force_unlock(&db_path).unwrap();
    let mut db = Database::open_path(&db_path).unwrap();
    db.execute("INSERT INTO t VALUES (1)").unwrap();
}

/// Same-host stale lock (old single-line format) keeps the legacy
/// reclaim behaviour — no regression for ordinary crash recovery.
#[test]
fn same_host_stale_lock_still_reclaims() {
    let dir = tmpdir("stale");
    let db_path = dir.join("t.spg");
    {
        let mut db = Database::open_path(&db_path).unwrap();
        db.execute("CREATE TABLE t (n BIGINT)").unwrap();
    }
    let lock = dir.join("t.spg.lock");
    std::fs::create_dir(&lock).unwrap();
    // Old format: pid only; 4_000_000 is far beyond pid_max on
    // macOS/Linux defaults, so the owner probes dead.
    std::fs::write(lock.join("pid"), "4000000").unwrap();
    let mut db = Database::open_path(&db_path).unwrap();
    db.execute("INSERT INTO t VALUES (1)").unwrap();
}

/// Round-23a — BIGSERIAL sequence addressability. The pg_dump
/// import idiom: restore rows with explicit ids, then
/// `setval(pg_get_serial_sequence(…), max_id)` so subsequent
/// app INSERTs don't collide with restored PKs.
#[test]
fn implicit_serial_sequence_addressable_after_import() {
    let mut db = spg_embedded::Database::open_in_memory();
    db.execute("CREATE TABLE messages (id BIGSERIAL PRIMARY KEY, subject TEXT)")
        .unwrap();
    // Restored rows carry explicit ids (as a dump does).
    db.execute("INSERT INTO messages (id, subject) VALUES (1,'a'),(2,'b'),(24304,'z')")
        .unwrap();
    // The dump's setval line — nested pg_get_serial_sequence form.
    let r = db
        .execute("SELECT setval(pg_get_serial_sequence('messages','id'), 24304, true)")
        .unwrap();
    match r {
        spg_embedded::QueryResult::Rows { rows, .. } => {
            assert_eq!(rows[0].values[0], spg_embedded::Value::Int(24304));
        }
        other => panic!("{other:?}"),
    }
    // App-style INSERT (no id) must continue ABOVE the restored max.
    db.execute("INSERT INTO messages (subject) VALUES ('new')")
        .unwrap();
    let r = db.execute("SELECT MAX(id) FROM messages").unwrap();
    match r {
        spg_embedded::QueryResult::Rows { rows, .. } => {
            assert_eq!(rows[0].values[0], spg_embedded::Value::BigInt(24305));
        }
        other => panic!("{other:?}"),
    }
    // currval/nextval address the implicit sequence directly.
    let r = db.execute("SELECT nextval('messages_id_seq')").unwrap();
    match r {
        spg_embedded::QueryResult::Rows { rows, .. } => {
            assert_eq!(rows[0].values[0], spg_embedded::Value::Int(24305));
        }
        other => panic!("{other:?}"),
    }
}

/// Round-23a — a table whose sequence was NEVER addressed keeps the
/// exact pre-7.29 max+1 behaviour.
#[test]
fn unaddressed_serial_keeps_max_plus_one() {
    let mut db = spg_embedded::Database::open_in_memory();
    db.execute("CREATE TABLE t (id BIGSERIAL PRIMARY KEY, x TEXT)")
        .unwrap();
    db.execute("INSERT INTO t (x) VALUES ('a'),('b')").unwrap();
    db.execute("DELETE FROM t WHERE id = 2").unwrap();
    db.execute("INSERT INTO t (x) VALUES ('c')").unwrap();
    let r = db.execute("SELECT MAX(id) FROM t").unwrap();
    match r {
        spg_embedded::QueryResult::Rows { rows, .. } => {
            assert_eq!(rows[0].values[0], spg_embedded::Value::BigInt(2));
        }
        other => panic!("{other:?}"),
    }
}
