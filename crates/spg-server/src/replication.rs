//! v4.24 single-master / multi-follower WAL streaming replication.
//!
//! ## Wire protocol (binary, framed)
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
use std::thread;
use std::time::Duration;

use spg_engine::Engine;

use crate::ServerState;

const MAGIC: &[u8; 8] = b"SPGREPL\x01";
/// Cadence for tailing the master WAL when no new bytes are present.
const TAIL_POLL: Duration = Duration::from_millis(50);
/// Cadence for the follower retry loop when the master is unreachable.
const RECONNECT_DELAY: Duration = Duration::from_millis(500);

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
fn serve_follower(mut stream: TcpStream, state: &ServerState) -> std::io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(30)))?;
    let mut hs = [0u8; 16];
    stream.read_exact(&mut hs)?;
    if &hs[..8] != MAGIC {
        return Err(std::io::Error::other("bad replication magic"));
    }
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
    tail_wal(stream, &wal_path, wal_position)
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

/// Tail `wal_path` from `start_offset` forever, streaming new bytes
/// to the follower as they appear. Polls every 50 ms when idle.
fn tail_wal(mut stream: TcpStream, wal_path: &Path, start_offset: u64) -> std::io::Result<()> {
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

    // Handshake.
    let mut hs = Vec::with_capacity(16);
    hs.extend_from_slice(MAGIC);
    hs.extend_from_slice(&start_offset.to_le_bytes());
    stream.write_all(&hs)?;
    stream.flush()?;

    // Initial reply.
    let mut len_buf = [0u8; 8];
    stream.read_exact(&mut len_buf)?;
    let snap_len = u64::from_le_bytes(len_buf);

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

    // Tail: read bytes, accumulate, apply complete records.
    let mut wal_appender = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(wal_path)?;
    let mut pending: Vec<u8> = Vec::with_capacity(4096);
    let mut chunk = [0u8; 4096];
    loop {
        let n = stream.read(&mut chunk)?;
        if n == 0 {
            return Ok(());
        }
        wal_appender.write_all(&chunk[..n])?;
        wal_appender.sync_data()?;
        pending.extend_from_slice(&chunk[..n]);
        // Drain complete records: [4-byte LE len][sql bytes].
        let mut cur = 0usize;
        while pending.len() - cur >= 4 {
            let len_bytes: [u8; 4] = pending[cur..cur + 4].try_into().unwrap();
            let rec_len = u32::from_le_bytes(len_bytes) as usize;
            if pending.len() - cur - 4 < rec_len {
                break;
            }
            let sql_bytes = &pending[cur + 4..cur + 4 + rec_len];
            let sql = core::str::from_utf8(sql_bytes)
                .map_err(|_| std::io::Error::other("non-UTF-8 SQL in replicated WAL record"))?;
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
            cur += 4 + rec_len;
        }
        if cur > 0 {
            pending.drain(0..cur);
        }
    }
}
