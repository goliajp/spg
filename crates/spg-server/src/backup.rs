//! v4.25 backup bundles + PITR helpers (CRC32 trailer added in
//! v4.37).
//!
//! A backup is a single self-contained file with this layout
//! (all integers `u64` little-endian):
//!
//! ```text
//! magic:    8 bytes  b"SPGBKUP\x01"  (v4.25, no CRC)
//!              -or-  b"SPGBKUP\x02"  (v4.37, trailing CRC32)
//! kind:     1 byte   0 = full, 1 = incremental
//! since:    8 bytes  WAL byte offset the previous full backup
//!                    captured up to (0 for full backups)
//! ts_micros: 8 bytes wall-clock micros at capture
//! snap_len: 8 bytes  catalog snapshot length (0 for incremental)
//! snap:     snap_len bytes
//! wal_pos:  8 bytes  WAL byte offset *at capture* (the end-marker
//!                    a subsequent incremental can use as `since`)
//! wal_len:  8 bytes  length of WAL bytes shipped in this bundle
//! wal:      wal_len bytes  (for a full backup: WAL[0..wal_pos];
//!                           for an incremental: WAL[since..wal_pos])
//! crc32:    4 bytes  CRC32 of every byte before it, present only
//!                    when magic == SPGBKUP\x02.
//! ```
//!
//! Operators stage a recovery by feeding `snap` into `db_path`
//! and the concatenated WAL slices into `wal_path`, then starting
//! spg-server. PITR with mid-bundle truncation uses the
//! `SPG_REPLAY_UPTO` env var (see main.rs).
//!
//! Writers always emit v2 from v4.37 on. Readers accept both: v1
//! loads unchanged (no CRC check); v2 verifies the trailing CRC32
//! and returns `BackupError::Corrupt` on mismatch — bit-flips in
//! the snapshot, WAL slice, or any header field surface as an
//! explicit error rather than silently corrupting the restore.

use std::io::Write;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::ServerState;

const MAGIC_V1: &[u8; 8] = b"SPGBKUP\x01";
const MAGIC_V2: &[u8; 8] = b"SPGBKUP\x02";
const KIND_FULL: u8 = 0;
const KIND_INCREMENTAL: u8 = 1;

#[derive(Debug)]
pub enum BackupError {
    Io(std::io::Error),
    NoWal,
    BadSinceOffset(u64),
    /// v4.37: v2-format bundle whose trailing CRC32 doesn't match
    /// the computed checksum of the body. Restoration is refused.
    #[allow(dead_code)]
    Corrupt {
        expected: u32,
        computed: u32,
    },
}

impl core::fmt::Display for BackupError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "backup io: {e}"),
            Self::NoWal => f.write_str("server has no WAL configured — backup requires WAL"),
            Self::BadSinceOffset(n) => {
                write!(f, "incremental SINCE offset {n} exceeds current WAL length")
            }
            Self::Corrupt { expected, computed } => write!(
                f,
                "backup bundle CRC32 mismatch (expected={expected:#010x}, computed={computed:#010x})"
            ),
        }
    }
}

impl From<std::io::Error> for BackupError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

/// Take a full backup. Returns the WAL position captured (the
/// `since` value a future incremental should pass).
///
/// The snapshot bytes already encode every write committed up to
/// `wal_pos`, so we deliberately ship `wal_len = 0` in the bundle
/// — replaying the WAL on top of the snapshot would duplicate
/// every CREATE / INSERT. The `wal_pos` field stays in the
/// envelope so a follow-up incremental can use it as `SINCE`.
pub fn take_full_backup(state: &ServerState, dest: &Path) -> Result<u64, BackupError> {
    let Some(wal_path) = state.wal_path.clone() else {
        return Err(BackupError::NoWal);
    };
    let (snapshot, wal_pos) = {
        let guard = state
            .engine
            .write()
            .map_err(|_| std::io::Error::other("engine lock poisoned"))?;
        let snap = guard.snapshot();
        drop(guard);
        let pos = std::fs::metadata(&wal_path).map_or(0, |m| m.len());
        (snap, pos)
    };
    write_bundle(dest, KIND_FULL, 0, &snapshot, wal_pos, &[])?;
    Ok(wal_pos)
}

/// Take an incremental backup: ship WAL bytes from `since` up to
/// the current end. No snapshot.
pub fn take_incremental_backup(
    state: &ServerState,
    dest: &Path,
    since: u64,
) -> Result<u64, BackupError> {
    let Some(wal_path) = state.wal_path.clone() else {
        return Err(BackupError::NoWal);
    };
    let (wal_bytes, wal_pos) = {
        let guard = state
            .engine
            .write()
            .map_err(|_| std::io::Error::other("engine lock poisoned"))?;
        let wb = std::fs::read(&wal_path)?;
        drop(guard);
        let pos = u64::try_from(wb.len()).unwrap_or(u64::MAX);
        (wb, pos)
    };
    let since_usize = usize::try_from(since).unwrap_or(usize::MAX);
    if since_usize > wal_bytes.len() {
        return Err(BackupError::BadSinceOffset(since));
    }
    let slice = &wal_bytes[since_usize..];
    write_bundle(dest, KIND_INCREMENTAL, since, &[], wal_pos, slice)?;
    Ok(wal_pos)
}

fn write_bundle(
    dest: &Path,
    kind: u8,
    since: u64,
    snapshot: &[u8],
    wal_pos: u64,
    wal_slice: &[u8],
) -> std::io::Result<()> {
    let ts = u64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_micros()),
    )
    .unwrap_or(u64::MAX);
    // v4.37: assemble in memory so the CRC32 trailer covers
    // every header + payload byte before it. Bundles are typically
    // small (snapshot blob + a WAL slice); the extra Vec doesn't
    // dominate. The single trailing CRC also lets restorers do
    // one syscall to read the file and check before applying.
    let mut body = Vec::with_capacity(
        MAGIC_V2.len() + 1 + 8 + 8 + 8 + snapshot.len() + 8 + 8 + wal_slice.len(),
    );
    body.extend_from_slice(MAGIC_V2);
    body.push(kind);
    body.extend_from_slice(&since.to_le_bytes());
    body.extend_from_slice(&ts.to_le_bytes());
    let snap_len = u64::try_from(snapshot.len()).unwrap_or(u64::MAX);
    body.extend_from_slice(&snap_len.to_le_bytes());
    body.extend_from_slice(snapshot);
    body.extend_from_slice(&wal_pos.to_le_bytes());
    let wal_len = u64::try_from(wal_slice.len()).unwrap_or(u64::MAX);
    body.extend_from_slice(&wal_len.to_le_bytes());
    body.extend_from_slice(wal_slice);
    let crc = spg_crypto::crc32::crc32(&body);
    let mut f = std::fs::File::create(dest)?;
    f.write_all(&body)?;
    f.write_all(&crc.to_le_bytes())?;
    f.sync_data()?;
    Ok(())
}

/// Inspect a bundle without applying it. v4.37 also verifies the
/// trailing CRC32 when the magic is `SPGBKUP\x02`; v1 bundles
/// inspect identically to before.
#[allow(dead_code)] // exposed for operator tooling and the e2e PITR test
pub fn inspect_bundle(path: &Path) -> std::io::Result<BundleHeader> {
    let bytes = std::fs::read(path)?;
    if bytes.len() < 8 + 1 + 8 + 8 + 8 + 8 + 8 {
        return Err(std::io::Error::other("bundle too short"));
    }
    let is_v2 = if &bytes[..8] == MAGIC_V1 {
        false
    } else if &bytes[..8] == MAGIC_V2 {
        true
    } else {
        return Err(std::io::Error::other("bad bundle magic"));
    };
    let kind = bytes[8];
    let since = u64::from_le_bytes(bytes[9..17].try_into().unwrap());
    let ts = u64::from_le_bytes(bytes[17..25].try_into().unwrap());
    let snap_len = u64::from_le_bytes(bytes[25..33].try_into().unwrap());
    let snap_end = 33 + usize::try_from(snap_len).unwrap_or(usize::MAX);
    if bytes.len() < snap_end + 16 {
        return Err(std::io::Error::other("bundle truncated in body"));
    }
    let wal_pos = u64::from_le_bytes(bytes[snap_end..snap_end + 8].try_into().unwrap());
    let wal_len = u64::from_le_bytes(bytes[snap_end + 8..snap_end + 16].try_into().unwrap());
    if is_v2 {
        let body_end = snap_end + 16 + usize::try_from(wal_len).unwrap_or(usize::MAX);
        if bytes.len() != body_end + 4 {
            return Err(std::io::Error::other(
                "v2 bundle: trailing CRC missing or extra bytes after CRC",
            ));
        }
        let expected = u32::from_le_bytes(bytes[body_end..body_end + 4].try_into().unwrap());
        let computed = spg_crypto::crc32::crc32(&bytes[..body_end]);
        if expected != computed {
            return Err(std::io::Error::other(format!(
                "backup bundle CRC32 mismatch (expected={expected:#010x}, computed={computed:#010x})"
            )));
        }
    }
    Ok(BundleHeader {
        kind,
        since,
        ts_micros: ts,
        snap_len,
        wal_pos,
        wal_len,
    })
}

#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct BundleHeader {
    pub kind: u8,
    pub since: u64,
    pub ts_micros: u64,
    pub snap_len: u64,
    pub wal_pos: u64,
    pub wal_len: u64,
}
