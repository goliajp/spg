//! v4.24 single-master / multi-follower WAL streaming replication
//! (extended in v4.36 with an opt-in framed protocol for lag-metric
//! observability).
//!
//! ## Wire protocol — v1 (legacy, magic `SPGREPL\x01`)
//!
//! Follower → Master handshake: 16 bytes
//!   - 8 bytes magic `b"SPGREPL\x01"`
//!   - 8 bytes `u64` LE — starting WAL offset (0 = full bootstrap)
//!
//! Master → Follower initial reply when offset = 0:
//!   - 8 bytes `u64` LE snapshot length (catalog / db file bytes)
//!   - `snapshot_len` bytes — raw db file
//!   - 8 bytes `u64` LE — starting WAL position the snapshot captures up to
//!
//! Master → Follower initial reply when offset > 0:
//!   - 8 bytes `u64` LE — `0` (no snapshot), follower already booted
//!
//! Then for the lifetime of the connection, the master streams raw WAL
//! bytes (the on-disk WAL is itself a sequence of `[u32 LE len][sql bytes]`
//! records, fsynced after each append). The follower buffers and applies
//! complete records via `Engine::execute`. No additional framing.
//!
//! ## Wire protocol — v2 (v4.36, magic `SPGREPL\x02`)
//!
//! Handshake is identical (just the magic byte differs). The initial
//! snapshot reply is identical too. After that, instead of raw WAL
//! bytes, the master emits a sequence of typed frames:
//!
//!   - 1 byte  — frame type (`0x00` = WAL chunk, `0x01` = status)
//!   - 4 bytes — `u32` LE payload length
//!   - N bytes — payload
//!
//! `0x00` WAL chunk: payload is opaque WAL bytes; the follower buffers
//! and applies complete `[u32 len][sql]` records exactly as in v1.
//!
//! `0x01` status frame: 16-byte payload — `[primary_wal_pos: u64 LE,
//! wall_time_us: u64 LE]`. The follower stores this in
//! `state.lag_state` so `/metrics` can expose
//! `spg_replication_lag_bytes` and `spg_replication_lag_seconds`.
//!
//! Status frames are advisory — dropping them never corrupts state.
//! Old v1 followers connect with the v1 magic and see no behavior
//! change; new v2 followers get the lag visibility for free. This is
//! the stable surface v4.36 adds to STABILITY.md.
//!
//! ## Scope cuts
//!
//! - No TLS, no auth, no failover, no sync replication. This is the same
//!   posture as the rest of v4.x: replication runs over a trusted network.
//! - No automated follower promotion. Operator stops the follower, points
//!   clients at it, restarts as a master.
//! - Snapshot capture takes the engine `RwLock` briefly to read the on-disk
//!   db file + WAL length atomically. The actual network send happens
//!   outside the lock so a slow follower cannot block writes.

use std::io::{Read, Seek, SeekFrom, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use spg_engine::Engine;

use crate::ServerState;

const MAGIC_V1: &[u8; 8] = b"SPGREPL\x01";
/// v4.36: framed protocol with periodic status frames carrying the
/// primary's current WAL position. Old clients keep using v1; new
/// clients send `\x02` and get lag visibility.
const MAGIC_V2: &[u8; 8] = b"SPGREPL\x02";
const FRAME_TYPE_WAL: u8 = 0x00;
const FRAME_TYPE_STATUS: u8 = 0x01;
/// Cadence for tailing the master WAL when no new bytes are present.
const TAIL_POLL: Duration = Duration::from_millis(50);
/// v4.36: cadence for periodic status-frame emission to v2 followers.
/// 50 ms matches the existing tail-poll loop so we piggyback in the
/// same idle slice rather than spinning up a separate timer thread.
const STATUS_INTERVAL: Duration = Duration::from_millis(50);
/// Cadence for the follower retry loop when the master is unreachable.
const RECONNECT_DELAY: Duration = Duration::from_millis(500);

/// v4.36: shared follower-side state populated from the master's
/// status frames. `spg_replication_lag_bytes` and
/// `spg_replication_lag_seconds` read from this; populated only when
/// the follower negotiated the v2 protocol. All zero on a v1 follower
/// — the /metrics path leaves the series out in that case.
#[derive(Debug)]
pub struct LagState {
    /// Most recent `primary_wal_pos` advertised by the master.
    pub primary_pos: AtomicU64,
    /// WAL offset the follower has applied through (matches the
    /// byte count of WAL the follower has fsynced + replayed).
    pub follower_applied_pos: AtomicU64,
    /// Wall-clock time (microseconds since UNIX epoch) the master
    /// stamped on its latest status frame. The follower uses
    /// `now() - this` for `spg_replication_lag_seconds`. Zero =
    /// no status frame seen yet, so /metrics omits the series.
    pub primary_wall_time_us: AtomicU64,
}

impl Default for LagState {
    fn default() -> Self {
        Self {
            primary_pos: AtomicU64::new(0),
            follower_applied_pos: AtomicU64::new(0),
            primary_wall_time_us: AtomicU64::new(0),
        }
    }
}

/// Spawn the master-side replication listener. Each accepted connection
/// runs in its own thread for the lifetime of the follower.
pub fn spawn_master_listener(
    addr: &str,
    state: Arc<ServerState>,
) -> std::io::Result<std::net::SocketAddr> {
    let listener = TcpListener::bind(addr)?;
    let local = listener.local_addr()?;
    thread::Builder::new()
        .name("spg-repl-listener".into())
        .spawn(move || {
            for stream in listener.incoming().flatten() {
                let state = Arc::clone(&state);
                thread::Builder::new()
                    .name("spg-repl-stream".into())
                    .spawn(move || {
                        if let Err(e) = serve_follower(stream, &state) {
                            eprintln!("spg-server: replication stream ended: {e}");
                        }
                    })
                    .ok();
            }
        })?;
    Ok(local)
}

/// Serve a single follower: handshake, snapshot, then tail the WAL.
/// Dispatches between the v1 raw-stream protocol and the v2 framed
/// protocol based on the handshake magic byte.
fn serve_follower(mut stream: TcpStream, state: &ServerState) -> std::io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(30)))?;
    let mut hs = [0u8; 16];
    stream.read_exact(&mut hs)?;
    let protocol = if &hs[..8] == MAGIC_V1 {
        Protocol::V1
    } else if &hs[..8] == MAGIC_V2 {
        Protocol::V2
    } else {
        return Err(std::io::Error::other("bad replication magic"));
    };
    let start_offset = u64::from_le_bytes(hs[8..16].try_into().unwrap());

    // Capture snapshot + WAL position under a brief lock so the
    // pair is consistent. If the follower already knows a starting
    // offset (resuming), we skip snapshot and tail from there.
    let (snapshot, wal_position) = if start_offset == 0 {
        capture_snapshot(state)?
    } else {
        (Vec::new(), start_offset)
    };

    if start_offset == 0 {
        // Initial reply: snapshot len + bytes + WAL start position.
        // Format unchanged between v1 and v2; the framed stream only
        // applies to the post-snapshot tail.
        let snap_len = u64::try_from(snapshot.len()).unwrap_or(u64::MAX);
        stream.write_all(&snap_len.to_le_bytes())?;
        if !snapshot.is_empty() {
            stream.write_all(&snapshot)?;
        }
        stream.write_all(&wal_position.to_le_bytes())?;
    } else {
        // No snapshot — signal with snap_len=0. The follower already
        // has the db file from a previous session.
        stream.write_all(&0_u64.to_le_bytes())?;
    }
    stream.flush()?;

    // Tail the WAL from `wal_position`.
    let Some(wal_path) = state.wal_path.clone() else {
        // No WAL configured → cannot stream further. Hold the
        // connection open in case future writes happen elsewhere.
        return Ok(());
    };
    match protocol {
        Protocol::V1 => tail_wal_v1(stream, &wal_path, wal_position),
        Protocol::V2 => tail_wal_v2(stream, &wal_path, wal_position),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Protocol {
    V1,
    V2,
}

/// Grab the in-memory engine state + WAL length atomically. The
/// lock is held only long enough to serialize the catalog and read
/// the WAL file size; the network send happens after release so a
/// slow follower can't block writers.
fn capture_snapshot(state: &ServerState) -> std::io::Result<(Vec<u8>, u64)> {
    let engine_guard = state
        .engine
        .write()
        .map_err(|_| std::io::Error::other("engine lock poisoned"))?;
    let snapshot = engine_guard.snapshot();
    let wal_position = match &state.wal_path {
        Some(p) if p.exists() => std::fs::metadata(p).map_or(0, |m| m.len()),
        _ => 0,
    };
    drop(engine_guard);
    Ok((snapshot, wal_position))
}

/// v1 tail: stream raw WAL bytes to the follower as they appear.
/// Polls every 50 ms when idle.
fn tail_wal_v1(mut stream: TcpStream, wal_path: &Path, start_offset: u64) -> std::io::Result<()> {
    let mut f = std::fs::File::open(wal_path)?;
    f.seek(SeekFrom::Start(start_offset))?;
    let mut buf = [0u8; 4096];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            thread::sleep(TAIL_POLL);
            continue;
        }
        stream.write_all(&buf[..n])?;
        stream.flush()?;
    }
}

/// v4.36: v2 tail. Frames WAL byte chunks as type `0x00`; emits a
/// type `0x01` status frame at least every `STATUS_INTERVAL` whether
/// or not new WAL bytes arrived. Status frames carry the master's
/// current WAL file size + wall-clock so the follower can compute
/// both `lag_bytes` (primary_pos − applied_pos) and `lag_seconds`
/// (now − last status wall time).
fn tail_wal_v2(mut stream: TcpStream, wal_path: &Path, start_offset: u64) -> std::io::Result<()> {
    let mut f = std::fs::File::open(wal_path)?;
    f.seek(SeekFrom::Start(start_offset))?;
    let mut current_offset = start_offset;
    let mut buf = [0u8; 4096];
    let mut last_status = std::time::Instant::now() - STATUS_INTERVAL;
    loop {
        let n = f.read(&mut buf)?;
        if n > 0 {
            write_frame(&mut stream, FRAME_TYPE_WAL, &buf[..n])?;
            current_offset = current_offset.saturating_add(n as u64);
        }
        // Send a status frame on the timer regardless of WAL activity,
        // or when we just made progress (so the follower's lag_bytes
        // tracks fresh data without waiting up to STATUS_INTERVAL).
        if n > 0 || last_status.elapsed() >= STATUS_INTERVAL {
            // Source-of-truth for primary_pos: the actual on-disk WAL
            // length, not the byte counter we maintain. They match
            // unless the file is being truncated under us, which the
            // engine doesn't do.
            let primary_pos = std::fs::metadata(wal_path).map_or(current_offset, |m| m.len());
            let wall_time_us = u64::try_from(
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map_or(0, |d| d.as_micros()),
            )
            .unwrap_or(0);
            let mut payload = [0u8; 16];
            payload[..8].copy_from_slice(&primary_pos.to_le_bytes());
            payload[8..].copy_from_slice(&wall_time_us.to_le_bytes());
            write_frame(&mut stream, FRAME_TYPE_STATUS, &payload)?;
            last_status = std::time::Instant::now();
        }
        if n == 0 {
            thread::sleep(TAIL_POLL);
        }
    }
}

fn write_frame(stream: &mut TcpStream, frame_type: u8, payload: &[u8]) -> std::io::Result<()> {
    let len = u32::try_from(payload.len())
        .map_err(|_| std::io::Error::other("replication frame payload too large"))?;
    let mut header = [0u8; 5];
    header[0] = frame_type;
    header[1..].copy_from_slice(&len.to_le_bytes());
    stream.write_all(&header)?;
    if !payload.is_empty() {
        stream.write_all(payload)?;
    }
    stream.flush()
}

/// Run as a follower: connect to `master`, fetch snapshot if our
/// db is empty, then tail WAL forever, applying each record. On
/// connect failure, reconnect after `RECONNECT_DELAY`. Designed to
/// be spawned in a dedicated thread holding `state` for shared
/// access to the engine `RwLock`.
#[allow(clippy::needless_pass_by_value)] // owned values keep the thread's `move ||` simple
pub fn run_follower(
    master_addr: String,
    db_path: PathBuf,
    wal_path: PathBuf,
    state: Arc<ServerState>,
) {
    loop {
        match follow_once(&master_addr, &db_path, &wal_path, &state) {
            Ok(()) => {
                eprintln!("spg-server: follower disconnected — retrying");
            }
            Err(e) => {
                eprintln!("spg-server: follower error: {e} — retrying");
            }
        }
        thread::sleep(RECONNECT_DELAY);
    }
}

fn follow_once(
    master_addr: &str,
    db_path: &Path,
    wal_path: &Path,
    state: &ServerState,
) -> std::io::Result<()> {
    let mut stream = TcpStream::connect(master_addr)?;
    stream.set_read_timeout(Some(Duration::from_secs(30)))?;

    // Determine our starting offset: 0 if no db yet, else current WAL length.
    let start_offset: u64 = if db_path.exists() && wal_path.exists() {
        std::fs::metadata(wal_path).map_or(0, |m| m.len())
    } else {
        0
    };

    // v4.36: always negotiate v2. Old masters reject v2 magic with
    // "bad replication magic" → caller retries via run_follower's
    // reconnect loop. The expected upgrade path is master-before-
    // follower (deploy v4.36 to the primary first); old v4.x clients
    // keep working via v1 magic on the master side.
    let mut hs = Vec::with_capacity(16);
    hs.extend_from_slice(MAGIC_V2);
    hs.extend_from_slice(&start_offset.to_le_bytes());
    stream.write_all(&hs)?;
    stream.flush()?;

    // Initial reply.
    let mut len_buf = [0u8; 8];
    stream.read_exact(&mut len_buf)?;
    let snap_len = u64::from_le_bytes(len_buf);

    let mut applied_offset = start_offset;
    if snap_len > 0 {
        // Receive snapshot.
        let mut snap = vec![
            0u8;
            usize::try_from(snap_len).map_err(|_| {
                std::io::Error::other("snapshot length exceeds usize range")
            })?
        ];
        stream.read_exact(&mut snap)?;
        std::fs::write(db_path, &snap)?;
        // Receive starting WAL position; pre-allocate the wal file
        // (we don't need its content — the master streams it).
        let mut pos_buf = [0u8; 8];
        stream.read_exact(&mut pos_buf)?;
        applied_offset = u64::from_le_bytes(pos_buf);
        std::fs::write(wal_path, b"")?;
        // Reload engine from new snapshot bytes.
        let new_engine = Engine::restore_envelope(&snap)
            .map_err(|e| std::io::Error::other(format!("follower restore from snapshot: {e}")))?;
        let mut g = state
            .engine
            .write()
            .map_err(|_| std::io::Error::other("engine lock poisoned"))?;
        *g = new_engine.with_clock(crate::wall_clock_micros);
    }
    state
        .lag_state
        .follower_applied_pos
        .store(applied_offset, Ordering::Release);

    // Tail: parse [u8 type][u32 len][payload] frames. WAL chunks
    // feed the existing record accumulator; status frames update
    // lag_state. The frame loop never errors on a status drop —
    // status is advisory — but any malformed framing kills the
    // connection so the reconnect loop can resync.
    let mut wal_appender = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(wal_path)?;
    let mut pending: Vec<u8> = Vec::with_capacity(4096);
    loop {
        // Frame header.
        let mut header = [0u8; 5];
        if let Err(e) = stream.read_exact(&mut header) {
            return if e.kind() == std::io::ErrorKind::UnexpectedEof {
                Ok(()) // clean disconnect
            } else {
                Err(e)
            };
        }
        let frame_type = header[0];
        let payload_len = u32::from_le_bytes(header[1..].try_into().unwrap()) as usize;
        let mut payload = vec![0u8; payload_len];
        if payload_len > 0 {
            stream.read_exact(&mut payload)?;
        }
        match frame_type {
            FRAME_TYPE_WAL => {
                wal_appender.write_all(&payload)?;
                wal_appender.sync_data()?;
                pending.extend_from_slice(&payload);
                // Drain complete records. v4.37+ records carry an
                // 8-byte header `[u32 (len | sentinel)][u32 crc32]`
                // and the CRC is verified; older records use the
                // bare 4-byte length header. The sentinel bit
                // distinguishes the two so a follower streams
                // mixed-format WAL cleanly through a mid-upgrade.
                let mut cur = 0usize;
                loop {
                    if pending.len() - cur < 4 {
                        break;
                    }
                    let len_bytes: [u8; 4] = pending[cur..cur + 4].try_into().unwrap();
                    let raw_len = u32::from_le_bytes(len_bytes);
                    let is_v2 = raw_len & crate::WAL_V2_SENTINEL != 0;
                    let rec_len = (raw_len & !crate::WAL_V2_SENTINEL) as usize;
                    let header_len = if is_v2 { 8 } else { 4 };
                    if pending.len() - cur < header_len + rec_len {
                        break;
                    }
                    let sql_bytes = &pending[cur + header_len..cur + header_len + rec_len];
                    if is_v2 {
                        let expected =
                            u32::from_le_bytes(pending[cur + 4..cur + 8].try_into().unwrap());
                        let actual = spg_crypto::crc32::crc32(sql_bytes);
                        if actual != expected {
                            return Err(std::io::Error::other(format!(
                                "replicated WAL CRC mismatch at follower offset {} (expected={expected:#010x}, computed={actual:#010x}, sql_len={rec_len})",
                                applied_offset.saturating_add(cur as u64)
                            )));
                        }
                    }
                    let sql = core::str::from_utf8(sql_bytes).map_err(|_| {
                        std::io::Error::other("non-UTF-8 SQL in replicated WAL record")
                    })?;
                    {
                        let mut eng = state
                            .engine
                            .write()
                            .map_err(|_| std::io::Error::other("engine lock poisoned"))?;
                        if let Err(e) = eng.execute(sql) {
                            return Err(std::io::Error::other(format!(
                                "follower apply rejected {sql:?}: {e}"
                            )));
                        }
                    }
                    cur += header_len + rec_len;
                    applied_offset = applied_offset.saturating_add((header_len + rec_len) as u64);
                }
                if cur > 0 {
                    pending.drain(0..cur);
                }
                state
                    .lag_state
                    .follower_applied_pos
                    .store(applied_offset, Ordering::Release);
            }
            FRAME_TYPE_STATUS => {
                if payload.len() == 16 {
                    let primary_pos = u64::from_le_bytes(payload[..8].try_into().unwrap());
                    let wall_time_us = u64::from_le_bytes(payload[8..].try_into().unwrap());
                    state
                        .lag_state
                        .primary_pos
                        .store(primary_pos, Ordering::Release);
                    state
                        .lag_state
                        .primary_wall_time_us
                        .store(wall_time_us, Ordering::Release);
                }
                // Unknown payload size on a known frame type: ignore.
                // The frame layout could grow in a future version and
                // older followers should tolerate the trailing bytes.
            }
            _ => {
                // Unknown frame type — same forward-compat rule:
                // skip the payload and keep going.
            }
        }
    }
}
