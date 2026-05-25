//! Append-only audit log with BLAKE3 hash chain.
//!
//! Each entry binds the previous entry's hash, so any tampering — modifying
//! a row, deleting an entry, reordering, splicing — surfaces on `verify()`.
//!
//! Entry layout on disk:
//!
//! ```text
//! +-----------+-----------+--------------+---------+---------+-----------+
//! | seq:u64   | ts_ms:u64 | prev:[u8;32] | hash:.. | sql_len | sql_bytes |
//! +-----------+-----------+--------------+---------+---------+-----------+
//! ```
//!
//! All multi-byte integers are little-endian. The file is prefixed with a
//! magic header + version byte to catch accidental misuse.
#![no_std]

extern crate alloc;

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt;

use spg_crypto::{OUT_LEN, hash as blake3};

pub const HASH_LEN: usize = OUT_LEN; // 32

const FILE_MAGIC: &[u8; 8] = b"SPGAUDIT";
const FILE_VERSION: u8 = 1;

/// One DML / DDL statement record, bound to the previous entry by hash.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditEntry {
    pub seq: u64,
    pub ts_ms: u64,
    pub prev_hash: [u8; HASH_LEN],
    pub hash: [u8; HASH_LEN],
    pub sql: String,
}

/// An in-memory audit log.
#[derive(Debug, Clone, Default)]
pub struct AuditLog {
    entries: Vec<AuditEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuditError {
    /// On-disk file is shorter than expected, or an integer / hash field
    /// is missing its tail bytes.
    Truncated,
    /// Magic / version don't match what we wrote.
    BadHeader(String),
    /// `prev_hash` doesn't match the previous entry's `hash`.
    BrokenChain { seq: u64 },
    /// `hash` doesn't equal `blake3(seq || ts || prev_hash || sql)` — the
    /// payload was tampered with.
    HashMismatch { seq: u64 },
    /// `sql_len` is non-UTF-8.
    InvalidUtf8 { seq: u64 },
    /// `sql_len` would overflow a `u32` or run off the buffer.
    BadLength { detail: String },
}

impl fmt::Display for AuditError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Truncated => f.write_str("audit log truncated"),
            Self::BadHeader(s) => write!(f, "audit log bad header: {s}"),
            Self::BrokenChain { seq } => write!(f, "audit log broken chain at seq {seq}"),
            Self::HashMismatch { seq } => write!(f, "audit log hash mismatch at seq {seq}"),
            Self::InvalidUtf8 { seq } => write!(f, "audit log invalid UTF-8 at seq {seq}"),
            Self::BadLength { detail } => write!(f, "audit log bad length: {detail}"),
        }
    }
}

impl AuditLog {
    pub const fn new() -> Self {
        Self {
            entries: Vec::new(),
        }
    }

    pub fn entries(&self) -> &[AuditEntry] {
        &self.entries
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Append a new entry. `sql` is the SQL text that was just executed;
    /// `ts_ms` is a wall-clock millisecond timestamp (caller-provided so
    /// `no_std` is preserved). Returns the new entry's hash for callers
    /// that want to log or pipe it elsewhere.
    pub fn append(&mut self, sql: String, ts_ms: u64) -> [u8; HASH_LEN] {
        let seq = self.entries.len() as u64;
        let prev_hash = self.entries.last().map_or([0u8; HASH_LEN], |e| e.hash);
        let hash = compute_entry_hash(seq, ts_ms, &prev_hash, sql.as_bytes());
        let entry = AuditEntry {
            seq,
            ts_ms,
            prev_hash,
            hash,
            sql,
        };
        self.entries.push(entry);
        hash
    }

    /// Verify every entry in turn:
    ///   * `seq` is its index (0, 1, 2 …)
    ///   * `prev_hash` == previous entry's hash (zero for the first entry)
    ///   * `hash` == BLAKE3(canonical payload)
    pub fn verify(&self) -> Result<(), AuditError> {
        for (i, e) in self.entries.iter().enumerate() {
            if e.seq != i as u64 {
                return Err(AuditError::BrokenChain { seq: e.seq });
            }
            let expected_prev = if i == 0 {
                [0u8; HASH_LEN]
            } else {
                self.entries[i - 1].hash
            };
            if e.prev_hash != expected_prev {
                return Err(AuditError::BrokenChain { seq: e.seq });
            }
            let recomputed = compute_entry_hash(e.seq, e.ts_ms, &e.prev_hash, e.sql.as_bytes());
            if recomputed != e.hash {
                return Err(AuditError::HashMismatch { seq: e.seq });
            }
        }
        Ok(())
    }

    /// Serialize the whole log (header + every entry).
    pub fn serialize(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(9 + self.entries.len() * 80);
        out.extend_from_slice(FILE_MAGIC);
        out.push(FILE_VERSION);
        for e in &self.entries {
            encode_entry(&mut out, e);
        }
        out
    }

    /// Serialize a single entry — exposed so the server can append one entry
    /// at a time to an open file handle without rewriting the whole log.
    pub fn encode_entry_to(&self, idx: usize, out: &mut Vec<u8>) {
        encode_entry(out, &self.entries[idx]);
    }

    /// Header bytes ([`FILE_MAGIC`] + [`FILE_VERSION`]). Useful for creating
    /// a fresh on-disk log file.
    #[must_use]
    pub fn header_bytes() -> Vec<u8> {
        let mut v = Vec::with_capacity(9);
        v.extend_from_slice(FILE_MAGIC);
        v.push(FILE_VERSION);
        v
    }

    /// Inverse of [`serialize`]. Also runs [`verify`] before returning.
    pub fn deserialize(buf: &[u8]) -> Result<Self, AuditError> {
        if buf.len() < 9 {
            return Err(AuditError::BadHeader(
                "buffer shorter than header (9 bytes)".into(),
            ));
        }
        if &buf[..8] != FILE_MAGIC {
            return Err(AuditError::BadHeader(format!(
                "wrong magic; got {:?}",
                &buf[..8]
            )));
        }
        if buf[8] != FILE_VERSION {
            return Err(AuditError::BadHeader(format!(
                "unsupported audit log version: {}",
                buf[8]
            )));
        }
        let mut log = Self::new();
        let mut off = 9;
        while off < buf.len() {
            let (entry, next) = decode_entry(buf, off)?;
            log.entries.push(entry);
            off = next;
        }
        log.verify()?;
        Ok(log)
    }
}

fn compute_entry_hash(seq: u64, ts: u64, prev: &[u8; HASH_LEN], sql: &[u8]) -> [u8; HASH_LEN] {
    let mut buf = Vec::with_capacity(8 + 8 + HASH_LEN + sql.len());
    buf.extend_from_slice(&seq.to_le_bytes());
    buf.extend_from_slice(&ts.to_le_bytes());
    buf.extend_from_slice(prev);
    buf.extend_from_slice(sql);
    blake3(&buf)
}

fn encode_entry(out: &mut Vec<u8>, e: &AuditEntry) {
    out.extend_from_slice(&e.seq.to_le_bytes());
    out.extend_from_slice(&e.ts_ms.to_le_bytes());
    out.extend_from_slice(&e.prev_hash);
    out.extend_from_slice(&e.hash);
    let sql_len = u32::try_from(e.sql.len()).expect("sql ≤ 4G");
    out.extend_from_slice(&sql_len.to_le_bytes());
    out.extend_from_slice(e.sql.as_bytes());
}

fn decode_entry(buf: &[u8], off: usize) -> Result<(AuditEntry, usize), AuditError> {
    // 8 (seq) + 8 (ts) + 32 (prev) + 32 (hash) + 4 (len) = 84 fixed bytes.
    let fixed_end = off.checked_add(84).ok_or_else(|| AuditError::BadLength {
        detail: "length overflow at fixed header".into(),
    })?;
    if buf.len() < fixed_end {
        return Err(AuditError::Truncated);
    }
    let seq = u64::from_le_bytes(buf[off..off + 8].try_into().expect("checked"));
    let ts_ms = u64::from_le_bytes(buf[off + 8..off + 16].try_into().expect("checked"));
    let mut prev_hash = [0u8; HASH_LEN];
    prev_hash.copy_from_slice(&buf[off + 16..off + 48]);
    let mut hash = [0u8; HASH_LEN];
    hash.copy_from_slice(&buf[off + 48..off + 80]);
    let sql_len = u32::from_le_bytes(buf[off + 80..off + 84].try_into().expect("checked"));
    let sql_end = fixed_end
        .checked_add(sql_len as usize)
        .ok_or_else(|| AuditError::BadLength {
            detail: format!("sql_len {sql_len} overflows offset"),
        })?;
    if buf.len() < sql_end {
        return Err(AuditError::Truncated);
    }
    let sql = core::str::from_utf8(&buf[fixed_end..sql_end])
        .map_err(|_| AuditError::InvalidUtf8 { seq })?
        .into();
    Ok((
        AuditEntry {
            seq,
            ts_ms,
            prev_hash,
            hash,
            sql,
        },
        sql_end,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::string::ToString;

    fn make_two_entry_log() -> AuditLog {
        let mut log = AuditLog::new();
        log.append("CREATE TABLE t (v INT)".into(), 1_000);
        log.append("INSERT INTO t VALUES (1)".into(), 1_005);
        log
    }

    #[test]
    fn empty_log_verifies() {
        assert!(AuditLog::new().verify().is_ok());
    }

    #[test]
    fn appended_entry_has_zero_prev_hash_for_first() {
        let mut log = AuditLog::new();
        log.append("any sql".into(), 42);
        assert_eq!(log.entries[0].seq, 0);
        assert_eq!(log.entries[0].prev_hash, [0u8; HASH_LEN]);
        assert_eq!(log.entries[0].ts_ms, 42);
    }

    #[test]
    fn second_entry_prev_hash_chains_to_first() {
        let log = make_two_entry_log();
        assert_eq!(log.entries[1].prev_hash, log.entries[0].hash);
        assert_ne!(log.entries[0].hash, log.entries[1].hash);
    }

    #[test]
    fn verify_passes_on_freshly_appended_log() {
        assert!(make_two_entry_log().verify().is_ok());
    }

    #[test]
    fn verify_detects_modified_sql() {
        let mut log = make_two_entry_log();
        log.entries[1].sql = "INSERT INTO t VALUES (2)".to_string();
        assert_eq!(log.verify(), Err(AuditError::HashMismatch { seq: 1 }));
    }

    #[test]
    fn verify_detects_broken_chain() {
        let mut log = make_two_entry_log();
        log.entries[1].prev_hash = [0u8; HASH_LEN];
        assert_eq!(log.verify(), Err(AuditError::BrokenChain { seq: 1 }));
    }

    #[test]
    fn serialize_deserialize_round_trip() {
        let log = make_two_entry_log();
        let bytes = log.serialize();
        let restored = AuditLog::deserialize(&bytes).expect("deserialize");
        assert_eq!(restored.entries, log.entries);
    }

    #[test]
    fn deserialize_rejects_bad_magic() {
        let mut bytes = make_two_entry_log().serialize();
        bytes[0] = b'X';
        assert!(matches!(
            AuditLog::deserialize(&bytes),
            Err(AuditError::BadHeader(_))
        ));
    }

    #[test]
    fn deserialize_rejects_tampered_entry_bytes() {
        let mut bytes = make_two_entry_log().serialize();
        // Flip a byte in the second entry's sql region. After header (9) +
        // entry 0 (84 + sql_len of entry 0). Easiest: flip byte at the very end.
        let last = bytes.len() - 1;
        bytes[last] ^= 0xFF;
        match AuditLog::deserialize(&bytes) {
            Err(AuditError::HashMismatch { .. } | AuditError::InvalidUtf8 { .. }) => {}
            other => panic!("expected HashMismatch or InvalidUtf8, got {other:?}"),
        }
    }

    #[test]
    fn deserialize_rejects_truncated_file() {
        let bytes = make_two_entry_log().serialize();
        let truncated = &bytes[..bytes.len() - 1];
        assert!(matches!(
            AuditLog::deserialize(truncated),
            Err(AuditError::Truncated)
        ));
    }
}
