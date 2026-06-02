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
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use spg_engine::Engine;
use spg_sql::ast::PublicationScope;

use crate::ServerState;

const MAGIC_V1: &[u8; 8] = b"SPGREPL\x01";
/// v6.1.4 — subscriber protocol magic. Distinct from MAGIC_V2 so
/// the master can:
///   - skip the snapshot dump (subscribers never want full state;
///     PG-style logical-replication subscribers expect target
///     tables to be present already)
///   - accept `start_offset = 0` as "tail from current end" rather
///     than "send full snapshot" (the publisher's effective start
///     position comes back in the handshake reply so the subscriber
///     can record it as its baseline `last_received_pos`)
/// Frame format on the post-handshake stream is identical to v2:
/// `[u8 type][u32 len][payload]` records keep working unchanged.
pub(crate) const MAGIC_SUB: &[u8; 8] = b"SPGSUB\x01\x00";
/// v4.36: framed protocol with periodic status frames carrying the
/// primary's current WAL position. Old clients keep using v1; new
/// clients send `\x02` and get lag visibility.
const MAGIC_V2: &[u8; 8] = b"SPGREPL\x02";
const FRAME_TYPE_WAL: u8 = 0x00;
const FRAME_TYPE_STATUS: u8 = 0x01;
/// v6.1.5 — `FRAME_TYPE_SKIP`. Master emits this on a MAGIC_SUB
/// stream when records the subscriber would otherwise have seen
/// were filtered out (not in any of the requested publications,
/// or DDL/session-control that v6.1.x logical replication never
/// propagates). Payload is `[u64 LE skipped_bytes]`; the
/// subscriber advances its `applied_offset` by that many bytes
/// without applying anything. Followers using MAGIC_V1 / MAGIC_V2
/// never receive this frame.
const FRAME_TYPE_SKIP: u8 = 0x02;
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
    } else if &hs[..8] == MAGIC_SUB {
        Protocol::Sub
    } else {
        return Err(std::io::Error::other("bad replication magic"));
    };
    let start_offset = u64::from_le_bytes(hs[8..16].try_into().unwrap());

    // v6.1.4: subscription protocol never sends a snapshot — the
    // subscriber's target tables exist already (per design point
    // 3 of `V6_1_DESIGN.md`, schema drift is the operator's
    // problem). `start_offset = 0` means "tail from the current
    // master WAL end"; non-zero means "resume from this byte".
    if matches!(protocol, Protocol::Sub) {
        // v6.1.5: subscription handshake grows a publication-name
        // tail so the master can filter records before sending.
        //   [u16 num_publications]
        //   for each: [u16 name_len][name bytes]
        // v6.1.6: subscriber appends its own 8-byte cluster_id
        // after the publication list. Backwards-compat with
        // v6.1.4: a subscriber that sent only the 16-byte magic +
        // offset would block here on the publication list read,
        // so v6.1.5-and-earlier masters are paired with v6.1.5+
        // subscribers; legacy v6.1.4 subscribers need a v6.1.4
        // master.
        let publication_names = read_publication_list(&mut stream)?;
        let mut sub_cluster_buf = [0u8; 8];
        stream.read_exact(&mut sub_cluster_buf)?;
        let subscriber_cluster_id = u64::from_le_bytes(sub_cluster_buf);
        let filter = build_publication_filter(state, &publication_names);

        let effective_start = if start_offset == 0 {
            current_wal_len(state)?
        } else {
            start_offset
        };
        // Reply: [u64 effective_start][u64 master_cluster_id].
        // The subscriber's REPLICATION_LOOP detection compares
        // these — direct cycle (subscriber subscribed to itself
        // or to a target that resolves to its own host).
        stream.write_all(&effective_start.to_le_bytes())?;
        stream.write_all(&state.cluster_id.to_le_bytes())?;
        stream.flush()?;
        // Belt-and-suspenders: if master can already see the loop
        // (it received its own cluster_id from the peer), log it
        // and bail without forwarding any records. The subscriber
        // also catches this in its reply parse; doing it here too
        // means the master doesn't waste WAL frames on a peer
        // that's about to drop the connection anyway.
        if subscriber_cluster_id == state.cluster_id {
            eprintln!(
                "spg-server: rejecting MAGIC_SUB connection — peer cluster_id matches own ({})",
                state.cluster_id
            );
            return Ok(());
        }
        let Some(wal_path) = state.wal_path.clone() else {
            return Ok(());
        };
        return tail_wal_v2_filtered(stream, &wal_path, effective_start, filter);
    }

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
        Protocol::V2 | Protocol::Sub => tail_wal_v2(stream, &wal_path, wal_position),
    }
}

/// v6.1.5 — read the publication-name list a subscriber appends
/// after its `start_offset`. Returns an empty Vec for a v6.1.4
/// subscriber (`num = 0`), letting the master treat that as the
/// fan-out-all-records legacy behaviour.
fn read_publication_list(stream: &mut TcpStream) -> std::io::Result<Vec<String>> {
    let mut num_buf = [0u8; 2];
    stream.read_exact(&mut num_buf)?;
    let num = u16::from_le_bytes(num_buf) as usize;
    let mut out = Vec::with_capacity(num);
    for _ in 0..num {
        let mut len_buf = [0u8; 2];
        stream.read_exact(&mut len_buf)?;
        let len = u16::from_le_bytes(len_buf) as usize;
        let mut name_buf = vec![0u8; len];
        if len > 0 {
            stream.read_exact(&mut name_buf)?;
        }
        let name = String::from_utf8(name_buf)
            .map_err(|e| std::io::Error::other(format!("publication name not UTF-8: {e}")))?;
        out.push(name);
    }
    Ok(out)
}

/// v6.1.5 — what a tail record's owner falls under for filter
/// purposes. The lightweight `extract_owner_from_sql` scanner
/// returns one of these; the filter combines per-record `Dml`s
/// with the publication scope to decide forward-vs-skip.
#[derive(Debug, Clone, PartialEq, Eq)]
enum OwnerKind {
    /// `INSERT INTO <name>` / `UPDATE <name>` / `DELETE FROM <name>`.
    /// The `<name>` is the table the record belongs to.
    Dml(String),
    /// Catalog or session-control SQL (CREATE / DROP / ALTER /
    /// TRUNCATE / BEGIN / COMMIT / ROLLBACK / SAVEPOINT / RELEASE /
    /// SET / SHOW). v6.1.5 policy: never propagate via logical
    /// replication. PG-compatible — PG's logical decoder also
    /// drops DDL.
    Skip,
}

/// v6.1.5 — flattened publication filter. A subscription can
/// request multiple publications; the master OR-combines their
/// scopes (a record is forwarded if ANY requested publication
/// accepts it). `AllTables` short-circuits the search.
#[derive(Debug, Clone)]
struct PublicationFilter {
    /// `true` when any requested publication is `AllTables`. Lets
    /// the filter answer "accept" in O(1) without walking the
    /// table sets.
    any_all_tables: bool,
    /// Collected `ForTables` allow-lists. A DML record's owner is
    /// accepted if it appears in any of these (deduped to a single
    /// `HashSet` for O(1) lookup).
    allow: std::collections::HashSet<String>,
    /// Collected `AllTablesExcept` deny-lists. The intersection of
    /// these is the "blocked" set — a record is accepted if its
    /// owner is in this `excepts_union` for *every* `AllTables
    /// Except` publication, i.e. blocked everywhere. But since
    /// scopes are OR'd, the accept rule is: at least one
    /// `AllTablesExcept` publication accepts the owner (i.e. owner
    /// NOT in its deny list). For correctness: store each deny
    /// list and check that *at least one* doesn't deny.
    deny_sets: Vec<std::collections::HashSet<String>>,
}

impl PublicationFilter {
    /// Accept everything — used for the legacy v6.1.4 path that
    /// sent `num_pubs = 0`. Behaviour identical to pre-v6.1.5.
    fn accept_all() -> Self {
        Self {
            any_all_tables: true,
            allow: std::collections::HashSet::new(),
            deny_sets: Vec::new(),
        }
    }

    fn accepts_owner(&self, owner: &str) -> bool {
        if self.any_all_tables {
            return true;
        }
        if self.allow.contains(owner) {
            return true;
        }
        // For `AllTablesExcept(deny)`: accept iff owner NOT in deny.
        // For the OR-of-publications rule, accept if at least one
        // deny set excludes the owner. (An empty `deny_sets` here
        // means none of the requested publications were `AllTables
        // Except` — so this arm contributes nothing.)
        self.deny_sets.iter().any(|deny| !deny.contains(owner))
    }
}

/// v6.1.5 — resolve a list of requested publication names against
/// the engine's `Publications` catalog, building a filter.
/// Unknown publication names (asked by the subscriber but not
/// declared on the master) log a warning and contribute nothing
/// — the rest of the requested set still applies.
fn build_publication_filter(state: &ServerState, names: &[String]) -> PublicationFilter {
    if names.is_empty() {
        // v6.1.4 subscriber sent no publications — preserve
        // pre-v6.1.5 fan-out-all behaviour.
        return PublicationFilter::accept_all();
    }
    let eng = match state.engine.read() {
        Ok(e) => e,
        Err(_) => return PublicationFilter::accept_all(),
    };
    let pubs = eng.publications();
    let mut filter = PublicationFilter {
        any_all_tables: false,
        allow: std::collections::HashSet::new(),
        deny_sets: Vec::new(),
    };
    for n in names {
        let Some(scope) = pubs.get(n) else {
            eprintln!(
                "spg-server: subscriber requested unknown publication {n:?} — \
                 contributes no records"
            );
            continue;
        };
        match scope {
            PublicationScope::AllTables => {
                filter.any_all_tables = true;
            }
            PublicationScope::ForTables(ts) => {
                for t in ts {
                    filter.allow.insert(t.clone());
                }
            }
            PublicationScope::AllTablesExcept(ts) => {
                filter.deny_sets.push(ts.iter().cloned().collect());
            }
        }
    }
    filter
}

/// v6.1.5 — Lightweight owner extractor. **Hot path** — runs once
/// per WAL record at the publisher's tail loop. Lexes only enough
/// of the SQL text to identify the verb (`INSERT` / `UPDATE` /
/// `DELETE` and friends) and, for DML, the immediately-following
/// table identifier. Unrecognised verbs fall to `Skip` (v6.1.x
/// logical replication propagates DML only — DDL / session
/// control / catalog mutations stay local; matches PG).
///
/// Worst-case cost is dominated by `to_ascii_uppercase` over the
/// first ~10 bytes; the budget is the ≤ 200 ns/record ship gate
/// from `V6_1_DESIGN.md` L2 row 5.
fn extract_owner_from_sql(sql: &str) -> OwnerKind {
    let s = sql.trim_start();
    let mut chars = s.bytes().enumerate();
    // Read the leading verb token without allocating: scan to the
    // first whitespace, then ASCII-fold to upper for comparison.
    let mut verb_end = s.len();
    for (i, b) in chars.by_ref() {
        if b.is_ascii_whitespace() {
            verb_end = i;
            break;
        }
    }
    if verb_end == 0 {
        return OwnerKind::Skip;
    }
    let verb = &s[..verb_end];
    // Macroscopic dispatch on the first letter then verify the
    // rest. Avoids a full string compare for the common `INSERT`.
    let upper_first = verb.as_bytes().first().map(|b| b.to_ascii_uppercase());
    let after_verb = s[verb_end..].trim_start();
    match upper_first {
        Some(b'I') if eq_ci(verb, b"INSERT") => {
            // INSERT INTO <name>
            let (kw, rest) = split_token(after_verb);
            if !eq_ci(kw, b"INTO") {
                return OwnerKind::Skip;
            }
            let (owner, _) = split_ident_token(rest.trim_start());
            if owner.is_empty() {
                OwnerKind::Skip
            } else {
                OwnerKind::Dml(strip_ident_punct(owner))
            }
        }
        Some(b'U') if eq_ci(verb, b"UPDATE") => {
            let (owner, _) = split_ident_token(after_verb);
            if owner.is_empty() {
                OwnerKind::Skip
            } else {
                OwnerKind::Dml(strip_ident_punct(owner))
            }
        }
        Some(b'D') if eq_ci(verb, b"DELETE") => {
            // DELETE FROM <name>
            let (kw, rest) = split_token(after_verb);
            if !eq_ci(kw, b"FROM") {
                return OwnerKind::Skip;
            }
            let (owner, _) = split_ident_token(rest.trim_start());
            if owner.is_empty() {
                OwnerKind::Skip
            } else {
                OwnerKind::Dml(strip_ident_punct(owner))
            }
        }
        // Everything else — DDL, session, catalog — never
        // propagated via logical replication.
        _ => OwnerKind::Skip,
    }
}

/// ASCII case-insensitive byte-slice compare. Used by the
/// lightweight owner scanner; saves allocating a lowercased
/// `String` per record.
fn eq_ci(a: &str, b_upper: &[u8]) -> bool {
    let ab = a.as_bytes();
    if ab.len() != b_upper.len() {
        return false;
    }
    for i in 0..ab.len() {
        if ab[i].to_ascii_uppercase() != b_upper[i] {
            return false;
        }
    }
    true
}

/// Split `s` at the first ASCII-whitespace boundary; returns
/// (head, rest). `head` is the token up to (not including) the
/// whitespace; `rest` is everything after.
fn split_token(s: &str) -> (&str, &str) {
    let bytes = s.as_bytes();
    for (i, b) in bytes.iter().enumerate() {
        if b.is_ascii_whitespace() {
            return (&s[..i], &s[i..]);
        }
    }
    (s, "")
}

/// Like `split_token` but also breaks at SQL punctuation that
/// follows a table identifier without whitespace, e.g.
/// `INSERT INTO bar(id) …`. Used by the owner scanner only.
fn split_ident_token(s: &str) -> (&str, &str) {
    let bytes = s.as_bytes();
    for (i, b) in bytes.iter().enumerate() {
        if b.is_ascii_whitespace() || matches!(*b, b'(' | b',' | b';') {
            return (&s[..i], &s[i..]);
        }
    }
    (s, "")
}

/// Strip leading/trailing characters a SQL identifier never carries
/// but that the splitter may have absorbed (quotes, `(`, `;`, `,`).
fn strip_ident_punct(s: &str) -> String {
    let mut end = s.len();
    while let Some(b) = s.as_bytes().get(end.wrapping_sub(1))
        && matches!(*b, b'(' | b';' | b',' | b'"' | b'\'')
    {
        end -= 1;
    }
    let mut start = 0usize;
    while let Some(b) = s.as_bytes().get(start)
        && matches!(*b, b'"' | b'\'')
    {
        start += 1;
    }
    s[start..end].to_string()
}

/// v6.1.4 — read the master's WAL file length under the engine
/// read-lock (so a concurrent write can't tear the read). Used as
/// the starting point when a subscriber connects with
/// `start_offset = 0`: tail from current end.
fn current_wal_len(state: &ServerState) -> std::io::Result<u64> {
    let Some(wal_path) = state.wal_path.as_ref() else {
        return Ok(0);
    };
    // Hold the engine read-lock briefly to fence against a
    // concurrent leader-commit round writing more WAL bytes mid-
    // stat. The lock is dropped immediately after.
    let _eng_guard = state
        .engine
        .read()
        .map_err(|_| std::io::Error::other("engine lock poisoned"))?;
    Ok(std::fs::metadata(wal_path).map_or(0, |m| m.len()))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Protocol {
    V1,
    V2,
    /// v6.1.4 — subscription protocol (`MAGIC_SUB`). No initial
    /// snapshot; v2-shaped frame stream after the
    /// effective-start-offset handshake reply.
    Sub,
}

/// Grab the in-memory engine state + WAL length atomically. The
/// lock is held only long enough to serialize the catalog and read
/// the WAL file size; the network send happens after release so a
/// slow follower can't block writers.
/// v6.0.x — sidecar `.applied_pos` file living next to the
/// follower's WAL. Holds 8 bytes LE = the master-WAL position
/// up to which the follower has applied records. Read at
/// `follow_once` entry to seed the in-memory atomic on a fresh
/// process; written atomically (temp + rename) after each frame's
/// apply batch.
fn applied_pos_sidecar_path(wal_path: &Path) -> PathBuf {
    let mut name = wal_path
        .file_name()
        .map(std::ffi::OsStr::to_os_string)
        .unwrap_or_default();
    name.push(".applied_pos");
    wal_path
        .parent()
        .map_or_else(|| PathBuf::from(&name), |p| p.join(&name))
}

fn applied_pos_sidecar_tmp_path(wal_path: &Path) -> PathBuf {
    let mut name = wal_path
        .file_name()
        .map(std::ffi::OsStr::to_os_string)
        .unwrap_or_default();
    name.push(".applied_pos.tmp");
    wal_path
        .parent()
        .map_or_else(|| PathBuf::from(&name), |p| p.join(&name))
}

fn read_applied_pos_sidecar(wal_path: &Path) -> Option<u64> {
    let bytes = std::fs::read(applied_pos_sidecar_path(wal_path)).ok()?;
    let arr: [u8; 8] = bytes.as_slice().try_into().ok()?;
    Some(u64::from_le_bytes(arr))
}

fn write_applied_pos_sidecar(wal_path: &Path, pos: u64) -> std::io::Result<()> {
    let tmp = applied_pos_sidecar_tmp_path(wal_path);
    let dst = applied_pos_sidecar_path(wal_path);
    std::fs::write(&tmp, pos.to_le_bytes())?;
    // POSIX rename within the same directory is atomic; Windows
    // tolerates this too. Either both files coexist briefly or only
    // `dst` does — no corrupted intermediate state visible to a
    // restarting follower reader.
    std::fs::rename(&tmp, &dst)?;
    Ok(())
}

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
/// both `lag_bytes` (`primary_pos` − `applied_pos`) and `lag_seconds`
/// (now − last status wall time).
fn tail_wal_v2(mut stream: TcpStream, wal_path: &Path, start_offset: u64) -> std::io::Result<()> {
    let mut f = std::fs::File::open(wal_path)?;
    f.seek(SeekFrom::Start(start_offset))?;
    let mut current_offset = start_offset;
    let mut buf = [0u8; 4096];
    let mut last_status = std::time::Instant::now()
        .checked_sub(STATUS_INTERVAL)
        .unwrap_or_else(std::time::Instant::now);
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

/// v6.1.5 — v2 tail variant that parses records out of the WAL
/// chunks and selectively forwards them based on the publication
/// filter. Records that don't match get a `FRAME_TYPE_SKIP` frame
/// instead so the subscriber's `applied_offset` still advances —
/// the publisher and subscriber stay in byte-position lockstep
/// regardless of how many records were filtered, which keeps the
/// reconnect path (start_offset = subscriber's last position)
/// efficient even on heavily-filtered streams.
#[allow(clippy::too_many_lines)] // record-parser + status timer share state; splitting scatters them
fn tail_wal_v2_filtered(
    mut stream: TcpStream,
    wal_path: &Path,
    start_offset: u64,
    filter: PublicationFilter,
) -> std::io::Result<()> {
    let mut f = std::fs::File::open(wal_path)?;
    f.seek(SeekFrom::Start(start_offset))?;
    let mut current_offset = start_offset;
    let mut buf = [0u8; 4096];
    let mut pending: Vec<u8> = Vec::with_capacity(4096);
    let mut last_status = std::time::Instant::now()
        .checked_sub(STATUS_INTERVAL)
        .unwrap_or_else(std::time::Instant::now);
    loop {
        let n = f.read(&mut buf)?;
        if n > 0 {
            pending.extend_from_slice(&buf[..n]);
            current_offset = current_offset.saturating_add(n as u64);
            // Drain complete records out of `pending`, forwarding
            // those the filter accepts and emitting SKIP frames
            // covering the contiguous spans of those it doesn't.
            // SKIP-coalescing lets a publication that matches 1%
            // of records still keep the subscriber-side wire
            // traffic small.
            let mut cur = 0usize;
            let mut skip_run_start: Option<usize> = None;
            loop {
                if pending.len() - cur < 4 {
                    break;
                }
                let len_bytes: [u8; 4] = pending[cur..cur + 4].try_into().unwrap();
                let raw_len = u32::from_le_bytes(len_bytes);
                let is_v2 = raw_len & crate::WAL_V2_SENTINEL != 0;
                let is_v3 = is_v2 && (raw_len & crate::WAL_V3_FLAG != 0);
                let len_mask = if is_v3 {
                    !(crate::WAL_V2_SENTINEL | crate::WAL_V3_FLAG)
                } else {
                    !crate::WAL_V2_SENTINEL
                };
                let rec_len = (raw_len & len_mask) as usize;
                let header_len = if is_v3 {
                    9
                } else if is_v2 {
                    8
                } else {
                    4
                };
                let total = header_len + rec_len;
                if pending.len() - cur < total {
                    break;
                }
                let sql_bytes = &pending[cur + header_len..cur + header_len + rec_len];
                // For v3 type-tag records, only `auto_commit_sql`
                // carries a SQL string we can extract owner from;
                // durability checkpoints are no-op markers (skip).
                let owner_kind = if is_v3 {
                    let type_byte = pending[cur + 8];
                    if type_byte == crate::WAL_V3_TYPE_AUTO_COMMIT_SQL {
                        match core::str::from_utf8(sql_bytes) {
                            Ok(s) => extract_owner_from_sql(s),
                            Err(_) => OwnerKind::Skip,
                        }
                    } else {
                        OwnerKind::Skip
                    }
                } else {
                    match core::str::from_utf8(sql_bytes) {
                        Ok(s) => extract_owner_from_sql(s),
                        Err(_) => OwnerKind::Skip,
                    }
                };
                let accept = match &owner_kind {
                    OwnerKind::Dml(owner) => filter.accepts_owner(owner),
                    OwnerKind::Skip => false,
                };
                if accept {
                    // Flush any pending SKIP run first.
                    if let Some(start) = skip_run_start.take() {
                        let skipped = (cur - start) as u64;
                        write_frame(&mut stream, FRAME_TYPE_SKIP, &skipped.to_le_bytes())?;
                    }
                    // Forward this record's bytes (header +
                    // payload) as a single WAL frame. The
                    // subscriber's parser slices them out exactly
                    // as if it had been streaming the raw WAL.
                    write_frame(&mut stream, FRAME_TYPE_WAL, &pending[cur..cur + total])?;
                } else if skip_run_start.is_none() {
                    skip_run_start = Some(cur);
                }
                cur += total;
            }
            // Trailing SKIP run that ran up to `cur` (the
            // start of the next partial record or end of pending).
            if let Some(start) = skip_run_start.take() {
                let skipped = (cur - start) as u64;
                write_frame(&mut stream, FRAME_TYPE_SKIP, &skipped.to_le_bytes())?;
            }
            if cur > 0 {
                pending.drain(0..cur);
            }
        }
        // Status frame on the same cadence as `tail_wal_v2`.
        if n > 0 || last_status.elapsed() >= STATUS_INTERVAL {
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

#[allow(clippy::too_many_lines)] // tight inline frame parser; splitting would scatter the v2-format branches
fn follow_once(
    master_addr: &str,
    db_path: &Path,
    wal_path: &Path,
    state: &ServerState,
) -> std::io::Result<()> {
    let mut stream = TcpStream::connect(master_addr)?;
    stream.set_read_timeout(Some(Duration::from_secs(30)))?;

    // v6.0.5+: start_offset is a position in MASTER's WAL file
    // (master `f.seek(SeekFrom::Start(start_offset))`), not a count
    // of bytes the follower has received locally. The right source
    // of truth for "next master-WAL byte to ship" is the AtomicU64
    // the apply loop maintains — and, for cross-process restart
    // where the in-memory atomic is fresh-zero, the sidecar
    // `.applied_pos` file written alongside the WAL. The sidecar
    // is updated after every apply batch (within the FRAME_TYPE_WAL
    // arm below), so on restart we recover the master-position from
    // disk and seed the atomic before the handshake.
    //
    // Caveat (filed for a future sub-version): the sidecar write
    // is NOT atomic with apply. If the process crashes between
    // apply and sidecar update, on restart the sidecar will lag
    // by ≤ one frame's records, master will re-send those, and
    // the follower will re-apply them. Non-idempotent SQL (no-PK
    // INSERTs) sees duplicate rows. Idempotent SQL (PK INSERTs,
    // CREATE TABLE IF NOT EXISTS) is unaffected.
    if state.lag_state.follower_applied_pos.load(Ordering::Acquire) == 0
        && let Some(persisted) = read_applied_pos_sidecar(wal_path)
        && persisted > 0
    {
        state
            .lag_state
            .follower_applied_pos
            .store(persisted, Ordering::Release);
    }
    let stored_applied = state.lag_state.follower_applied_pos.load(Ordering::Acquire);
    let start_offset: u64 = if db_path.exists() && stored_applied > 0 {
        stored_applied
    } else if db_path.exists() && wal_path.exists() {
        // Last-ditch fallback for the very-rare case where the
        // sidecar got lost but db + wal survived. Resume from
        // local WAL length; works only when master's wal_position
        // was 0 at the first handshake. Otherwise master will
        // seek mid-record and the drain loop will misalign — the
        // exact v6.0.5 bug we just fixed. Logged loud so ops can
        // spot it.
        let n = std::fs::metadata(wal_path).map_or(0, |m| m.len());
        if n > 0 {
            eprintln!(
                "spg-server: follower sidecar .applied_pos missing — \
                 falling back to wal length {n}; this is byte-exact \
                 only if master's wal_position was 0 at first handshake"
            );
        }
        n
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
    // v6.0.x: persist the post-snapshot applied_pos so that even a
    // restart immediately after the initial handshake (before any
    // tail frames arrive) recovers without a full re-snapshot.
    if let Err(e) = write_applied_pos_sidecar(wal_path, applied_offset) {
        eprintln!(
            "spg-server: follower sidecar write failed at handshake offset {applied_offset}: {e}"
        );
    }

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
                // Drain complete records. Three on-disk formats coexist
                // (same shape `replay_wal_bytes` accepts at startup):
                //   v1 (≤v4.36): 4-byte len, no CRC, bit 31 = 0.
                //   v2 (v4.37+): 4-byte (len|0x8000_0000) + 4-byte
                //                CRC over payload, bit 31 = 1, bit 30 = 0.
                //   v3 (v4.41+): 4-byte (len|0xC000_0000) + 4-byte CRC
                //                over [type||payload] + 1-byte type,
                //                bit 31 = 1, bit 30 = 1. `len` counts
                //                payload, not the type byte.
                // The sentinel bits distinguish all three so a follower
                // streams mixed-format WAL cleanly through a mid-upgrade.
                let mut cur = 0usize;
                loop {
                    if pending.len() - cur < 4 {
                        break;
                    }
                    let len_bytes: [u8; 4] = pending[cur..cur + 4].try_into().unwrap();
                    let raw_len = u32::from_le_bytes(len_bytes);
                    let is_v2 = raw_len & crate::WAL_V2_SENTINEL != 0;
                    let is_v3 = is_v2 && (raw_len & crate::WAL_V3_FLAG != 0);
                    let len_mask = if is_v3 {
                        !(crate::WAL_V2_SENTINEL | crate::WAL_V3_FLAG)
                    } else {
                        !crate::WAL_V2_SENTINEL
                    };
                    let rec_len = (raw_len & len_mask) as usize;
                    // v1 = 4-byte header; v2 = 4+4; v3 = 4+4+1 (type byte).
                    let header_len = if is_v3 {
                        9
                    } else if is_v2 {
                        8
                    } else {
                        4
                    };
                    if pending.len() - cur < header_len + rec_len {
                        break;
                    }
                    let payload_off = cur + header_len;
                    let sql_bytes = &pending[payload_off..payload_off + rec_len];
                    if is_v2 {
                        let expected =
                            u32::from_le_bytes(pending[cur + 4..cur + 8].try_into().unwrap());
                        let actual = if is_v3 {
                            // v3 CRC covers `[type byte || payload]`.
                            let type_byte = pending[cur + 8];
                            let mut buf = Vec::with_capacity(1 + sql_bytes.len());
                            buf.push(type_byte);
                            buf.extend_from_slice(sql_bytes);
                            spg_crypto::crc32::crc32(&buf)
                        } else {
                            spg_crypto::crc32::crc32(sql_bytes)
                        };
                        if actual != expected {
                            return Err(std::io::Error::other(format!(
                                "replicated WAL CRC mismatch at follower offset {} (expected={expected:#010x}, computed={actual:#010x}, payload_len={rec_len})",
                                applied_offset.saturating_add(cur as u64)
                            )));
                        }
                    }
                    if is_v3 {
                        // v3 dispatches on the type tag. `auto_commit_sql`
                        // is the only kind v4.41 emits; unknown bytes are
                        // fatal — never silently skipped (same forward-
                        // compat fence as `replay_wal_bytes`).
                        let type_byte = pending[cur + 8];
                        match type_byte {
                            crate::WAL_V3_TYPE_AUTO_COMMIT_SQL => {}
                            other => {
                                return Err(std::io::Error::other(format!(
                                    "replicated WAL v3 unknown type byte {other:#04x} at follower offset {} — refusing to apply",
                                    applied_offset.saturating_add(cur as u64)
                                )));
                            }
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
                // v6.0.x: persist applied_pos to sidecar so cross-
                // process restart resumes from the right master-WAL
                // byte without going through a fresh snapshot. One
                // sidecar write per frame's apply batch keeps the
                // disk-cost amortised; the apply / sidecar gap is
                // bounded by the bytes in `pending` after the drain
                // (which is ≤ one record header + payload).
                if let Err(e) = write_applied_pos_sidecar(wal_path, applied_offset) {
                    eprintln!(
                        "spg-server: follower sidecar write failed at offset {applied_offset}: {e}"
                    );
                }
            }
            FRAME_TYPE_STATUS if payload.len() == 16 => {
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
            _ => {
                // Two forward-compat cases collapsed:
                // - FRAME_TYPE_STATUS with a size we don't recognise
                //   (the layout could grow in a future version)
                // - completely unknown frame type
                // Either way: skip the payload and keep going.
            }
        }
    }
}

// ---- v6.1.4 subscriber worker ----------------------------------

/// Frequency the worker checks its shutdown flag while blocked on
/// the read socket. Short enough that DROP SUBSCRIPTION feels
/// instant in tests; long enough not to pummel the kernel.
const SUB_READ_TIMEOUT: Duration = Duration::from_millis(500);

/// v6.1.4 — parse PG keyword=value connection string for `host=…
/// port=…`. Other keywords are accepted but ignored (forward-compat
/// surface for v6.1.x options). Returns `(host, port)` or an
/// error string the worker logs verbatim.
fn parse_conn_str(s: &str) -> Result<(String, u16), String> {
    let mut host: Option<String> = None;
    let mut port: Option<u16> = None;
    for tok in s.split_ascii_whitespace() {
        let Some((k, v)) = tok.split_once('=') else {
            return Err(format!("expected key=value token, got {tok:?}"));
        };
        match k.to_ascii_lowercase().as_str() {
            "host" => host = Some(v.to_string()),
            "port" => {
                port = Some(
                    v.parse::<u16>()
                        .map_err(|e| format!("bad port {v:?}: {e}"))?,
                );
            }
            // Forward-compat: ignore unknown keys (user, password,
            // sslmode, application_name, etc.). v6.1.4 only needs
            // host+port.
            _ => {}
        }
    }
    let host = host.ok_or_else(|| "conn_str missing host=…".to_string())?;
    let port = port.ok_or_else(|| "conn_str missing port=…".to_string())?;
    Ok((host, port))
}

/// v6.1.4 — entry point for a single subscription's background
/// thread. Reconnects on failure with `RECONNECT_DELAY` between
/// attempts; exits cleanly when `shutdown` flips to true.
pub fn run_subscription_worker(
    name: String,
    conn_str: String,
    state: Arc<ServerState>,
    shutdown: Arc<AtomicBool>,
) {
    while !shutdown.load(Ordering::Acquire) {
        match subscribe_once(&name, &conn_str, &state, &shutdown) {
            Ok(()) => {
                if shutdown.load(Ordering::Acquire) {
                    return;
                }
                eprintln!("spg-server: subscription {name:?} disconnected — retrying");
            }
            Err(e) => {
                eprintln!("spg-server: subscription {name:?} error: {e} — retrying");
            }
        }
        // Sleep a few short ticks rather than one long sleep so
        // DROP SUBSCRIPTION feels responsive even mid-reconnect.
        let mut slept = Duration::ZERO;
        while slept < RECONNECT_DELAY {
            if shutdown.load(Ordering::Acquire) {
                return;
            }
            thread::sleep(SUB_READ_TIMEOUT);
            slept += SUB_READ_TIMEOUT;
        }
    }
}

/// One subscription connect-and-drain attempt. Returns `Ok(())`
/// on clean disconnect (caller decides whether to reconnect based
/// on the shutdown flag); returns `Err` on any IO / framing /
/// engine-apply failure.
#[allow(clippy::too_many_lines)] // tight frame parser; the v6.1.5 filter pass will live here too
fn subscribe_once(
    name: &str,
    conn_str: &str,
    state: &Arc<ServerState>,
    shutdown: &Arc<AtomicBool>,
) -> std::io::Result<()> {
    let (host, port) = parse_conn_str(conn_str).map_err(std::io::Error::other)?;
    let addr = format!("{host}:{port}");
    let mut stream = TcpStream::connect(&addr)?;
    stream.set_read_timeout(Some(SUB_READ_TIMEOUT))?;

    // Worker reads its own row to learn where to resume +
    // which publication name(s) to request from the master.
    // If the subscription was dropped mid-spawn (race against
    // reconcile), bail out cleanly.
    let (start_offset, requested_publications) = {
        let eng = state
            .engine
            .read()
            .map_err(|_| std::io::Error::other("engine lock poisoned"))?;
        match eng.subscriptions().get(name) {
            Some(s) => (s.last_received_pos, s.publications.clone()),
            None => return Ok(()),
        }
    };

    // MAGIC_SUB + start_offset + publication-name list +
    // subscriber's own cluster_id (v6.1.6 addition).
    // [8 bytes magic]
    // [8 bytes offset]
    // [2 bytes num_pubs] for each: [2 bytes len][len bytes]
    // [8 bytes subscriber_cluster_id]
    let mut hs = Vec::with_capacity(
        16 + 2 + requested_publications.iter().map(|p| 2 + p.len()).sum::<usize>() + 8,
    );
    hs.extend_from_slice(MAGIC_SUB);
    hs.extend_from_slice(&start_offset.to_le_bytes());
    let num_pubs = u16::try_from(requested_publications.len()).map_err(|_| {
        std::io::Error::other("subscription requests too many publications (max 65,535)")
    })?;
    hs.extend_from_slice(&num_pubs.to_le_bytes());
    for p in &requested_publications {
        let len = u16::try_from(p.len()).map_err(|_| {
            std::io::Error::other("publication name too long (max 65,535 bytes)")
        })?;
        hs.extend_from_slice(&len.to_le_bytes());
        hs.extend_from_slice(p.as_bytes());
    }
    hs.extend_from_slice(&state.cluster_id.to_le_bytes());
    stream.write_all(&hs)?;
    stream.flush()?;

    // Reply: [u64 effective_start][u64 master_cluster_id].
    let mut reply = [0u8; 16];
    read_exact_with_shutdown(&mut stream, &mut reply, shutdown)?;
    let mut applied_offset = u64::from_le_bytes(reply[..8].try_into().unwrap());
    let master_cluster_id = u64::from_le_bytes(reply[8..].try_into().unwrap());
    if master_cluster_id == state.cluster_id {
        // v6.1.6 — direct self-loop. Master's cluster_id equals
        // our own. Logging here, and returning Err so the
        // reconnect-loop emits a visible signal. Operator must
        // DROP SUBSCRIPTION to silence the noise.
        eprintln!(
            "spg-server: subscription {name:?}: REPLICATION_LOOP — master cluster_id \
             {master_cluster_id} matches own; aborting link"
        );
        return Err(std::io::Error::other("REPLICATION_LOOP"));
    }

    // Tail loop. Same frame format as v6.0.x follower. The chief
    // differences: no local WAL file write, advance the
    // engine's subscription `last_received_pos` instead of
    // `lag_state.follower_applied_pos`, and exit on the
    // shutdown flag between frame reads.
    let mut pending: Vec<u8> = Vec::with_capacity(4096);
    loop {
        if shutdown.load(Ordering::Acquire) {
            return Ok(());
        }
        let mut header = [0u8; 5];
        if !read_exact_with_shutdown(&mut stream, &mut header, shutdown)? {
            return Ok(());
        }
        let frame_type = header[0];
        let payload_len = u32::from_le_bytes(header[1..].try_into().unwrap()) as usize;
        let mut payload = vec![0u8; payload_len];
        if payload_len > 0
            && !read_exact_with_shutdown(&mut stream, &mut payload, shutdown)?
        {
            return Ok(());
        }
        match frame_type {
            FRAME_TYPE_WAL => {
                pending.extend_from_slice(&payload);
                let mut cur = 0usize;
                loop {
                    if pending.len() - cur < 4 {
                        break;
                    }
                    let len_bytes: [u8; 4] = pending[cur..cur + 4].try_into().unwrap();
                    let raw_len = u32::from_le_bytes(len_bytes);
                    let is_v2 = raw_len & crate::WAL_V2_SENTINEL != 0;
                    let is_v3 = is_v2 && (raw_len & crate::WAL_V3_FLAG != 0);
                    let len_mask = if is_v3 {
                        !(crate::WAL_V2_SENTINEL | crate::WAL_V3_FLAG)
                    } else {
                        !crate::WAL_V2_SENTINEL
                    };
                    let rec_len = (raw_len & len_mask) as usize;
                    let header_len = if is_v3 {
                        9
                    } else if is_v2 {
                        8
                    } else {
                        4
                    };
                    if pending.len() - cur < header_len + rec_len {
                        break;
                    }
                    let payload_off = cur + header_len;
                    let sql_bytes = &pending[payload_off..payload_off + rec_len];
                    if is_v2 {
                        let expected =
                            u32::from_le_bytes(pending[cur + 4..cur + 8].try_into().unwrap());
                        let actual = if is_v3 {
                            let type_byte = pending[cur + 8];
                            let mut buf = Vec::with_capacity(1 + sql_bytes.len());
                            buf.push(type_byte);
                            buf.extend_from_slice(sql_bytes);
                            spg_crypto::crc32::crc32(&buf)
                        } else {
                            spg_crypto::crc32::crc32(sql_bytes)
                        };
                        if actual != expected {
                            return Err(std::io::Error::other(format!(
                                "subscription {name:?} WAL CRC mismatch at offset {} \
                                 (expected={expected:#010x}, computed={actual:#010x}, \
                                 payload_len={rec_len})",
                                applied_offset.saturating_add(cur as u64)
                            )));
                        }
                    }
                    if is_v3 {
                        let type_byte = pending[cur + 8];
                        match type_byte {
                            crate::WAL_V3_TYPE_AUTO_COMMIT_SQL => {}
                            // v6.1.4 silently skips durability-checkpoint
                            // markers (no engine state to mutate). v6.1.5
                            // will treat unknown types as fatal once
                            // publication-filtered streams stabilise.
                            crate::WAL_V3_TYPE_DURABILITY_CHECKPOINT => {
                                cur += header_len + rec_len;
                                applied_offset =
                                    applied_offset.saturating_add((header_len + rec_len) as u64);
                                continue;
                            }
                            other => {
                                return Err(std::io::Error::other(format!(
                                    "subscription {name:?}: unknown WAL v3 type byte \
                                     {other:#04x} at offset {} — refusing to apply",
                                    applied_offset.saturating_add(cur as u64)
                                )));
                            }
                        }
                    }
                    let sql = core::str::from_utf8(sql_bytes).map_err(|_| {
                        std::io::Error::other("non-UTF-8 SQL in subscribed WAL record")
                    })?;
                    let record_size = (header_len + rec_len) as u64;
                    let new_pos = applied_offset.saturating_add(cur as u64) + record_size;
                    // Apply + advance under the engine write-lock so
                    // the SQL execution and the position update are
                    // observed together by SHOW SUBSCRIPTIONS.
                    {
                        let mut eng = state
                            .engine
                            .write()
                            .map_err(|_| std::io::Error::other("engine lock poisoned"))?;
                        // v6.1.4 ignores subscription-side errors that
                        // duplicate idempotent DDL (CREATE TABLE that
                        // happens to exist already). Anything else
                        // surfaces and kills the connection so the
                        // worker can reconnect with a clean state.
                        if let Err(e) = eng.execute(sql) {
                            // Subscription-friendly tolerant apply:
                            // "table already exists", "duplicate" → log
                            // and continue. Anything else bails so the
                            // operator notices.
                            let msg = format!("{e:?}");
                            let tolerant = msg.contains("DuplicateTable")
                                || msg.contains("DuplicateIndex")
                                || msg.contains("DuplicateUser")
                                || msg.contains("AlreadyExists");
                            if !tolerant {
                                return Err(std::io::Error::other(format!(
                                    "subscription {name:?} apply rejected {sql:?}: {msg}"
                                )));
                            }
                            eprintln!(
                                "spg-server: subscription {name:?} tolerating apply error \
                                 on {sql:?}: {msg}"
                            );
                        }
                        if !eng.subscription_advance(name, new_pos) {
                            // The subscription was dropped mid-stream.
                            return Ok(());
                        }
                    }
                    cur += header_len + rec_len;
                    applied_offset = applied_offset.saturating_add((header_len + rec_len) as u64);
                }
                if cur > 0 {
                    pending.drain(0..cur);
                }
            }
            FRAME_TYPE_STATUS => {
                // v6.1.4 ignores status frames — they're advisory
                // for SHOW REPLICATION LAG on followers; the
                // subscriber materialises its own progress via
                // SHOW SUBSCRIPTIONS.
            }
            FRAME_TYPE_SKIP => {
                // v6.1.5 — master filtered out N bytes' worth of
                // records (publication scope rejected or DDL).
                // Advance `applied_offset` and `last_received_pos`
                // to stay in lock-step with the master's WAL
                // position, so a future reconnect requests the
                // right start byte.
                if payload.len() == 8 {
                    let skipped = u64::from_le_bytes(payload[..8].try_into().unwrap());
                    applied_offset = applied_offset.saturating_add(skipped);
                    let mut eng = state
                        .engine
                        .write()
                        .map_err(|_| std::io::Error::other("engine lock poisoned"))?;
                    if !eng.subscription_advance(name, applied_offset) {
                        return Ok(());
                    }
                }
            }
            _ => {
                // Unknown frame type — forward-compat skip.
            }
        }
    }
}

/// Helper around `read_exact` that returns `Ok(false)` instead of
/// erroring on a clean `UnexpectedEof` AND treats a read timeout
/// as a "check shutdown then keep waiting" point. The 500 ms read
/// timeout means a DROP SUBSCRIPTION takes at most ~500 ms to
/// shut the worker down even mid-receive.
fn read_exact_with_shutdown(
    stream: &mut TcpStream,
    buf: &mut [u8],
    shutdown: &Arc<AtomicBool>,
) -> std::io::Result<bool> {
    let mut got = 0usize;
    while got < buf.len() {
        if shutdown.load(Ordering::Acquire) {
            return Ok(false);
        }
        match stream.read(&mut buf[got..]) {
            Ok(0) => return Ok(false), // peer closed
            Ok(n) => got += n,
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                // Read-timeout tick — loop and re-check shutdown.
            }
            Err(e) => return Err(e),
        }
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_owner_insert_into_table() {
        assert_eq!(
            extract_owner_from_sql("INSERT INTO foo VALUES (1)"),
            OwnerKind::Dml("foo".to_string())
        );
        // Quoted ident
        assert_eq!(
            extract_owner_from_sql("INSERT INTO \"Foo\" VALUES (1)"),
            OwnerKind::Dml("Foo".to_string())
        );
        // Lowercase + trailing punctuation
        assert_eq!(
            extract_owner_from_sql("insert  into  bar(id) values (1)"),
            OwnerKind::Dml("bar".to_string())
        );
    }

    #[test]
    fn extract_owner_update_delete() {
        assert_eq!(
            extract_owner_from_sql("UPDATE users SET x=1 WHERE id=2"),
            OwnerKind::Dml("users".to_string())
        );
        assert_eq!(
            extract_owner_from_sql("DELETE FROM users WHERE id=2"),
            OwnerKind::Dml("users".to_string())
        );
    }

    #[test]
    fn extract_owner_ddl_is_skip() {
        for sql in [
            "CREATE TABLE t (id INT)",
            "DROP TABLE t",
            "ALTER INDEX idx REBUILD",
            "TRUNCATE t",
            "BEGIN",
            "COMMIT",
            "ROLLBACK",
            "SAVEPOINT sp1",
            "RELEASE SAVEPOINT sp1",
            "SET search_path = public",
            "CREATE PUBLICATION p FOR ALL TABLES",
            "DROP PUBLICATION p",
            "CREATE SUBSCRIPTION s CONNECTION 'h=x' PUBLICATION p",
            "CREATE USER 'alice' WITH PASSWORD 'x'",
        ] {
            assert_eq!(
                extract_owner_from_sql(sql),
                OwnerKind::Skip,
                "expected Skip for {sql:?}"
            );
        }
    }

    #[test]
    fn extract_owner_garbage_is_skip() {
        assert_eq!(extract_owner_from_sql(""), OwnerKind::Skip);
        assert_eq!(extract_owner_from_sql("   "), OwnerKind::Skip);
        // INSERT not followed by INTO
        assert_eq!(
            extract_owner_from_sql("INSERT VALUES (1)"),
            OwnerKind::Skip
        );
        // INSERT INTO with no table name
        assert_eq!(extract_owner_from_sql("INSERT INTO"), OwnerKind::Skip);
    }

    #[test]
    fn extract_owner_perf_under_200ns() {
        // V6_1_DESIGN.md L2 row 5 ship gate: ≤ 200 ns/record for
        // the lightweight owner scanner. We time 10K iterations to
        // amortise instant() noise; on Apple-M release it lands
        // well under the budget. Marked #[ignore] so a noisy host
        // doesn't fail CI; run with `--ignored` to verify.
        const ITERS: u32 = 10_000;
        let sql = "INSERT INTO some_table_name VALUES (1, 'hello world', 3.14)";
        let t0 = std::time::Instant::now();
        for _ in 0..ITERS {
            let r = std::hint::black_box(extract_owner_from_sql(std::hint::black_box(sql)));
            std::hint::black_box(r);
        }
        let ns_per_call = t0.elapsed().as_nanos() / u128::from(ITERS);
        eprintln!("extract_owner_from_sql: {ns_per_call} ns/call (budget ≤ 200 ns)");
        assert!(
            ns_per_call < 200,
            "owner scanner exceeded the v6.1.5 200 ns budget: {ns_per_call} ns/call"
        );
    }

    #[test]
    fn publication_filter_accept_all_matches_everything() {
        let f = PublicationFilter::accept_all();
        assert!(f.accepts_owner("t1"));
        assert!(f.accepts_owner("anything"));
    }

    #[test]
    fn publication_filter_for_tables_allow_list() {
        let mut f = PublicationFilter {
            any_all_tables: false,
            allow: std::collections::HashSet::new(),
            deny_sets: Vec::new(),
        };
        f.allow.insert("t1".to_string());
        f.allow.insert("t3".to_string());
        assert!(f.accepts_owner("t1"));
        assert!(!f.accepts_owner("t2"));
        assert!(f.accepts_owner("t3"));
    }

    #[test]
    fn publication_filter_all_tables_except_deny_list() {
        let mut deny = std::collections::HashSet::new();
        deny.insert("bad".to_string());
        let f = PublicationFilter {
            any_all_tables: false,
            allow: std::collections::HashSet::new(),
            deny_sets: vec![deny],
        };
        assert!(!f.accepts_owner("bad"));
        assert!(f.accepts_owner("good"));
    }

    #[test]
    fn publication_filter_or_combines_multiple_scopes() {
        // A subscription requesting two publications:
        // - FOR TABLE t1
        // - FOR ALL TABLES EXCEPT bad
        // OR-combine: accept if either accepts.
        let mut allow = std::collections::HashSet::new();
        allow.insert("t1".to_string());
        let mut deny = std::collections::HashSet::new();
        deny.insert("bad".to_string());
        let f = PublicationFilter {
            any_all_tables: false,
            allow,
            deny_sets: vec![deny],
        };
        assert!(f.accepts_owner("t1")); // matched by ForTables
        assert!(f.accepts_owner("anything_else")); // accepted by AllTablesExcept (not in deny)
        assert!(!f.accepts_owner("bad")); // denied by AllTablesExcept AND not allowed by ForTables
    }
}
