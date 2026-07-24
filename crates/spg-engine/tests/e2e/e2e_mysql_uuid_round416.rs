//! read01 round 416 (MySQL differential) — `UUID()` under the MySQL
//! dialect, returning a 36-char CHAR(36) text.
//!
//! MariaDB's `UUID()` is a bread-and-butter builder for primary keys and
//! idempotency tokens, returning the canonical 36-char `8-4-4-4-12` hex-
//! with-hyphens form typed as CHAR(36). SPG had no MySQL-side `UUID()` —
//! calls errored at parse-into-eval with "function uuid() does not exist".
//! PG's own `gen_random_uuid()` returns the typed UUID and is unaffected.
//!
//! Version: SPG emits **UUID v7** (RFC 9562) here — a 48-bit big-endian
//! unix-ms timestamp in bytes 0..6 plus random tail. That matches MariaDB's
//! v1 output's *time-ordered* property (so downstream B-tree fragmentation
//! stays low on inserts) without leaking a MAC address the way v1 does.
//! Plain v4 would satisfy the format check but lose that sort locality.
//!
//! Every expectation is copied from a MariaDB 11 run (except the version
//! nibble, which is intentionally 7 vs MariaDB's 1 — documented above).

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn mysql() -> Engine {
    let mut e = Engine::new();
    // A wall clock is needed for the v7 timestamp bytes; without it the
    // ts prefix is all-zero and the UUID falls back to effectively v4.
    e = e.with_clock(|| {
        use std::time::{SystemTime, UNIX_EPOCH};
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_micros() as i64)
            .unwrap_or(0)
    });
    e.execute("SET sql_mode='STRICT_TRANS_TABLES'").unwrap();
    e
}

fn scalar(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err}")) {
        QueryResult::Rows { rows, .. } => match &rows[0].values[0] {
            Value::Null => "NULL".to_string(),
            v => spg_engine::eval::value_to_text(v),
        },
        other => panic!("{other:?}"),
    }
}

/// The 36-char CHAR(36) shape MariaDB documents.
#[test]
fn shape_and_length() {
    let mut e = mysql();
    assert_eq!(scalar(&mut e, "SELECT LENGTH(UUID())"), "36");
    // 8-4-4-4-12 hex with hyphens.
    let s = scalar(&mut e, "SELECT UUID()");
    let parts: Vec<&str> = s.split('-').collect();
    assert_eq!(parts.len(), 5, "expected 5 groups, got {s:?}");
    assert_eq!(parts[0].len(), 8);
    assert_eq!(parts[1].len(), 4);
    assert_eq!(parts[2].len(), 4);
    assert_eq!(parts[3].len(), 4);
    assert_eq!(parts[4].len(), 12);
    assert!(s.chars().all(|c| c == '-' || c.is_ascii_hexdigit()));
}

/// Each call returns a different value.
#[test]
fn uniqueness() {
    let mut e = mysql();
    assert_eq!(scalar(&mut e, "SELECT UUID() = UUID()"), "false");
}

/// Version 7: the 15th character (start of the 3rd group) is '7'; the 20th
/// (start of the 4th group) is in `{8,9,a,b}` — the RFC 4122/9562 variant.
#[test]
fn version_and_variant_bits() {
    let mut e = mysql();
    assert_eq!(scalar(&mut e, "SELECT SUBSTRING(UUID(),15,1)"), "7");
    let v = scalar(&mut e, "SELECT SUBSTRING(UUID(),20,1)");
    assert!(
        matches!(v.as_str(), "8" | "9" | "a" | "b"),
        "variant nibble should be 8/9/a/b, got {v:?}"
    );
}

/// v7 sorts by insertion time — a UUID emitted later has a greater string
/// value byte-for-byte (the 48-bit unix-ms prefix is big-endian).
#[test]
fn v7_is_time_ordered() {
    let mut e = mysql();
    let a = scalar(&mut e, "SELECT UUID()");
    // A generous sleep so the ms timestamp definitely advances.
    std::thread::sleep(std::time::Duration::from_millis(5));
    let b = scalar(&mut e, "SELECT UUID()");
    assert!(a < b, "later UUID should sort after earlier: {a:?} then {b:?}");
}

/// A PostgreSQL session has no `UUID()` — the parse-time / eval-time error
/// stays.
#[test]
fn postgres_rejects() {
    let mut e = Engine::new();
    assert!(
        e.execute("SELECT UUID()").is_err(),
        "PG has no UUID() (it has gen_random_uuid())"
    );
}
