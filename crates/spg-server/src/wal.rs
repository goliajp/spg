//! WAL v2/v3 record codec, the leader-batched commit queue, and WAL
//! replay. Lifted out of `main.rs` (server file split). Re-exported at
//! the crate root via `pub(crate) use wal::*` so `crate::append_wal`,
//! `crate::WAL_V2_SENTINEL`, etc. keep resolving for flusher / replication.

use std::env;
use std::io::Write;
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};

use spg_engine::{Engine, QueryResult};

use crate::{
    CommitResult, CommitTask, DEFAULT_COMMIT_GROUP_MAX, ServerState, parse_env_u64, parse_env_usize,
};
use crate::{observability, pubsub};

/// WAL record format (sentinel-bit framing across versions):
///   v1 (≤ v4.36): `[u32 LE len][len bytes]`                                bit 31 = 0
///   v2 (v4.37+):  `[u32 LE (len | 0x8000_0000)][u32 LE crc32][len bytes]`  bit 31 = 1, bit 30 = 0
///   v3 (v4.41+):  `[u32 LE (len | 0xC000_0000)][u32 LE crc32][1 byte type][len bytes payload]`
///                                                                          bit 31 = 1, bit 30 = 1
///
/// v1 lengths are << 2 GiB in practice so bit 31 was free for the
/// v2 sentinel; v2 lengths are << 1 GiB in practice so bit 30 was
/// free for v3. `len` in the v3 frame counts only the `payload`
/// body (the leading type byte is fixed header overhead, kept out
/// of `len` so the quota math stays simple).
///
/// The CRC32 in v3 covers `[type byte || payload]` — the type byte
/// is integrity-protected too. Unknown type bytes during replay
/// return a hard error (no silent skip).
///
/// Old v4.x binaries reading v3 records crash on the "huge len" —
/// forward-compat isn't required by STABILITY (clients only need
/// to read older formats).
pub(crate) const WAL_V2_SENTINEL: u32 = 0x8000_0000;
pub(crate) const WAL_V3_FLAG: u32 = 0x4000_0000;
pub(crate) const WAL_V3_SENTINEL: u32 = WAL_V2_SENTINEL | WAL_V3_FLAG;

/// v5.4.2 — cached `SPG_SYNCHRONOUS_COMMIT` parse. Returns `true`
/// when async-commit mode is opted in (`SPG_SYNCHRONOUS_COMMIT` ∈
/// {`off`, `false`, `0`}, case-insensitive). The result is cached
/// behind `OnceLock` because the env is read once per process; a
/// benchmark that flips the knob must restart the server.
///
/// In async mode the WAL write path skips `sync_data` — the
/// flusher thread (v5.4.1) handles durability via periodic
/// `durability_checkpoint` markers. The opt-in keyword set is
/// the same one `FlusherConfig::from_env` recognises, so a
/// misread env stays consistent across both modules.
pub(crate) fn synchronous_commit_disabled() -> bool {
    static CACHED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *CACHED.get_or_init(|| {
        std::env::var("SPG_SYNCHRONOUS_COMMIT")
            .is_ok_and(|s| matches!(s.trim().to_lowercase().as_str(), "off" | "false" | "0"))
    })
}

/// v4.41 v3 record type tags. Reserve a byte rather than a bit so
/// future record kinds (binary INSERT, multi-row batch, snapshot
/// marker) can all share the v3 frame without another sentinel.
pub(crate) const WAL_V3_TYPE_AUTO_COMMIT_SQL: u8 = 0x01;
/// v5.4.0 — durability checkpoint marker. Payload is `[u64 LE
/// byte_offset]`, the WAL byte position where this marker frame
/// starts (i.e. how many bytes of WAL preceded it). Semantics:
/// "every WAL byte before this marker had successfully reached
/// `fsync` at the time the marker was written." The flusher
/// thread in async-commit mode (v5.4.1+) emits one every N
/// records or N microseconds. Replay treats this as a no-op
/// (engine state isn't mutated); the marker is purely metadata
/// for crash-recovery debugging and chaos tests that need to
/// know how much of an async-commit window was durable on kill.
pub(crate) const WAL_V3_TYPE_DURABILITY_CHECKPOINT: u8 = 0x02;

/// v6.6.1 — LZSS-compressed auto-commit SQL. Payload layout:
///   `[u8 algo][compressed bytes]`
/// where `algo = 0x01` reserves room for v6.x to add LZ4 / zstd
/// without another type-tag bump. The compressed bytes are
/// `spg_crypto::lzss::compress(sql.as_bytes())`. Replay decompresses
/// and routes through `Engine::execute` exactly like type 0x01.
pub(crate) const WAL_V3_TYPE_COMPRESSED_SQL: u8 = 0x03;
pub(crate) const WAL_COMPRESS_ALGO_LZSS: u8 = 0x01;
/// Compression threshold (bytes). SQL payloads smaller than this
/// skip the encoder — LZSS overhead doesn't pay off below ~256 B.
/// Operator-tunable via `SPG_COMPRESSION_MIN_BYTES` env (v6.6.3).
pub(crate) const WAL_COMPRESS_MIN_BYTES: usize = 256;

pub(crate) fn encode_wal_record(sql: &str) -> std::io::Result<Vec<u8>> {
    let len = u32::try_from(sql.len())
        .map_err(|_| std::io::Error::other("SQL too large for WAL entry"))?;
    if len & WAL_V2_SENTINEL != 0 {
        return Err(std::io::Error::other(
            "SQL byte count would alias the v4.37 WAL framing sentinel (≥ 2 GiB)",
        ));
    }
    let crc = spg_crypto::crc32::crc32(sql.as_bytes());
    let mut entry = Vec::with_capacity(8 + sql.len());
    entry.extend_from_slice(&(len | WAL_V2_SENTINEL).to_le_bytes());
    entry.extend_from_slice(&crc.to_le_bytes());
    entry.extend_from_slice(sql.as_bytes());
    Ok(entry)
}

/// v4.41 v3 encoder. `payload` is the body bytes (semantics
/// depend on `type_tag`); the returned slice is the framed record
/// `[sentinel|len][crc32(type||payload)][type][payload]`. The CRC
/// covers `type` so a corrupted type byte fails the replay check.
pub(crate) fn encode_wal_v3_record(type_tag: u8, payload: &[u8]) -> std::io::Result<Vec<u8>> {
    let len = u32::try_from(payload.len())
        .map_err(|_| std::io::Error::other("WAL v3 payload too large"))?;
    // bit 30 + bit 31 are reserved; payload < 1 GiB in practice
    // covers any auto-commit SQL or per-INSERT binary batch we ship.
    if len & (WAL_V2_SENTINEL | WAL_V3_FLAG) != 0 {
        return Err(std::io::Error::other(
            "WAL v3 payload size would alias the v4.41 sentinel bits (≥ 1 GiB)",
        ));
    }
    let mut crc_input = Vec::with_capacity(1 + payload.len());
    crc_input.push(type_tag);
    crc_input.extend_from_slice(payload);
    let crc = spg_crypto::crc32::crc32(&crc_input);
    let mut entry = Vec::with_capacity(9 + payload.len());
    entry.extend_from_slice(&(len | WAL_V3_SENTINEL).to_le_bytes());
    entry.extend_from_slice(&crc.to_le_bytes());
    entry.push(type_tag);
    entry.extend_from_slice(payload);
    Ok(entry)
}

/// v6.6.1 — encode an auto-commit SQL record, applying LZSS
/// compression when the payload would benefit. Falls back to the
/// uncompressed v3 type=0x01 path when:
///   - SPG_WAL_COMPRESSION env is `none`
///   - SQL bytes < SPG_COMPRESSION_MIN_BYTES env (default 256)
///   - LZSS output isn't actually smaller than input (pathological)
/// Returns the framed record bytes ready for WAL append.
///
/// v6.6.3 — increments `Metrics.wal_bytes_uncompressed_in` and
/// `wal_bytes_compressed_out` so the `/metrics` endpoint can
/// derive the live ratio.
pub(crate) fn encode_wal_auto_commit_sql_metrics(
    sql: &str,
    metrics: &observability::Metrics,
) -> std::io::Result<Vec<u8>> {
    use std::sync::atomic::Ordering;
    let raw_len = sql.len() as u64;
    metrics
        .wal_bytes_uncompressed_in
        .fetch_add(raw_len, Ordering::Relaxed);
    let threshold = wal_compression_min_bytes();
    if !wal_compression_enabled() || sql.len() < threshold {
        let out = encode_wal_v3_record(WAL_V3_TYPE_AUTO_COMMIT_SQL, sql.as_bytes())?;
        metrics
            .wal_bytes_compressed_out
            .fetch_add(out.len() as u64, Ordering::Relaxed);
        return Ok(out);
    }
    let compressed = spg_crypto::lzss::compress(sql.as_bytes());
    // Compressed payload = [algo byte][compressed bytes]. Compare
    // against the uncompressed SQL length to decide.
    if compressed.len() + 1 >= sql.len() {
        let out = encode_wal_v3_record(WAL_V3_TYPE_AUTO_COMMIT_SQL, sql.as_bytes())?;
        metrics
            .wal_bytes_compressed_out
            .fetch_add(out.len() as u64, Ordering::Relaxed);
        return Ok(out);
    }
    let mut payload = Vec::with_capacity(1 + compressed.len());
    payload.push(WAL_COMPRESS_ALGO_LZSS);
    payload.extend_from_slice(&compressed);
    let out = encode_wal_v3_record(WAL_V3_TYPE_COMPRESSED_SQL, &payload)?;
    metrics
        .wal_bytes_compressed_out
        .fetch_add(out.len() as u64, Ordering::Relaxed);
    Ok(out)
}

/// v6.6.1 — encode without metrics. Used in test paths and the
/// few callers that don't have ServerState handy. Production
/// commit_queue path uses `_metrics`.
#[allow(dead_code)]
pub(crate) fn encode_wal_auto_commit_sql(sql: &str) -> std::io::Result<Vec<u8>> {
    let threshold = wal_compression_min_bytes();
    if !wal_compression_enabled() || sql.len() < threshold {
        return encode_wal_v3_record(WAL_V3_TYPE_AUTO_COMMIT_SQL, sql.as_bytes());
    }
    let compressed = spg_crypto::lzss::compress(sql.as_bytes());
    if compressed.len() + 1 >= sql.len() {
        return encode_wal_v3_record(WAL_V3_TYPE_AUTO_COMMIT_SQL, sql.as_bytes());
    }
    let mut payload = Vec::with_capacity(1 + compressed.len());
    payload.push(WAL_COMPRESS_ALGO_LZSS);
    payload.extend_from_slice(&compressed);
    encode_wal_v3_record(WAL_V3_TYPE_COMPRESSED_SQL, &payload)
}

/// v6.6.3 — operator-tunable threshold (bytes). SQL payloads
/// smaller than this skip LZSS. Default 256; env-tunable via
/// `SPG_COMPRESSION_MIN_BYTES`. Cached after first call.
pub(crate) fn wal_compression_min_bytes() -> usize {
    static CHECKED: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *CHECKED.get_or_init(|| {
        std::env::var("SPG_COMPRESSION_MIN_BYTES")
            .ok()
            .and_then(|s| s.parse::<usize>().ok())
            .unwrap_or(WAL_COMPRESS_MIN_BYTES)
    })
}

/// v6.6.1 — runtime check of `SPG_WAL_COMPRESSION` env. Default
/// `lzss` (enabled). `none` disables. Cached after first call.
/// v7.39 (round 194) — WAL compression is now OPT-IN
/// (`SPG_WAL_COMPRESSION=lzss`), default off. The v6.6.1 default-on
/// put the LZSS encoder on the commit critical path INSIDE the engine
/// write lock: compressing a 24 KB multi-row VALUES statement costs
/// ~12 ms (~2 MB/s), 6× the write itself — the r194 wire panel showed
/// insert_batch_1k at 14.6 ms vs 2.3 ms with compression off, for a
/// saving of ~14 KB whose write cost is microseconds. Disk-constrained
/// operators can still opt in; an LZSS throughput attack is the
/// recorded follow-up that could earn the default back.
pub(crate) fn wal_compression_enabled() -> bool {
    static CHECKED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *CHECKED.get_or_init(|| {
        std::env::var("SPG_WAL_COMPRESSION")
            .is_ok_and(|v| v.eq_ignore_ascii_case("lzss") || v.eq_ignore_ascii_case("on"))
    })
}

/// v4.41 single-record byte total for the v3 auto-commit wrap.
/// 9 bytes of header (4 sentinel+len + 4 CRC + 1 type) plus the
/// SQL payload. Replaces the v4.34 three-v2-record block
/// (`8+5 BEGIN + 8+sql + 8+6 COMMIT` = 35 + sql bytes) with
/// 9 + sql bytes — same quota check, smaller footprint.
pub(crate) fn wal_v3_auto_commit_size(sql: &str) -> u64 {
    9u64 + sql.len() as u64
}

/// v5.4.0 — encode a `durability_checkpoint` v3 record. Payload
/// is the 8-byte LE WAL byte offset where this marker frame
/// starts (i.e. the WAL file length *before* this marker is
/// appended). The framed wrap is the standard v3 envelope:
///
///   `[u32 (len=8 | 0xC000_0000)] [u32 crc32(type || payload)] [type=0x02] [u64 LE byte_offset]`
///
/// Total frame size = 17 bytes. CRC covers `[type || payload]`,
/// matching every other v3 frame.
pub(crate) fn encode_durability_marker(byte_offset: u64) -> std::io::Result<Vec<u8>> {
    encode_wal_v3_record(
        WAL_V3_TYPE_DURABILITY_CHECKPOINT,
        &byte_offset.to_le_bytes(),
    )
}

/// v5.4.0 — append one `durability_checkpoint` marker to the WAL
/// and `sync_data` so the marker plus every byte preceding it is
/// confirmed durable. Returns the WAL byte offset where the marker
/// frame started (= recorded `byte_offset` payload), so callers
/// (the flusher thread in v5.4.1+) can update durability-lag
/// metrics by diffing against the WAL's current end-of-file.
///
/// Shares the same quota / `wal_min_free_bytes` water-mark check
/// the auto-commit write path (`append_wal_v3_group`) runs — a
/// marker that violates the disk-full chaos contract fails the
/// same way an INSERT would, so the flusher thread can degrade
/// gracefully. No-WAL servers return `Ok(0)` (nothing to mark).
///
/// v5.4.4 — lock-free fsync. The marker bytes are written under
/// the `wal` mutex (microseconds), then the mutex is released
/// **before** `sync_data` is called via `wal_sync_clone` (a
/// `try_clone`'d handle to the same underlying file). The OS sees
/// both descriptors as the same file; `sync_data` works on the
/// file's data without needing exclusive access. This decouples
/// the flusher's fsync latency (~5 ms on macOS APFS) from the
/// client write path, restoring the v5.4.2 async-commit throughput
/// promise — without this fix the flusher mutex monopolises the
/// WAL and client INSERTs back up behind fsync (real bug observed
/// in the v5.4.4 smoke test: async mode 9× SLOWER than sync).
pub(crate) fn append_durability_marker(state: &ServerState) -> std::io::Result<u64> {
    let Some(wal_mutex) = state.wal.as_ref() else {
        return Ok(0);
    };
    let pre_marker_offset = {
        let mut wal = wal_mutex
            .lock()
            .map_err(|_| std::io::Error::other("wal mutex poisoned"))?;
        let pre_marker_offset = wal.metadata()?.len();
        let entry = encode_durability_marker(pre_marker_offset)?;
        if let Some(quota) = state.chaos.wal_quota_bytes
            && pre_marker_offset.saturating_add(entry.len() as u64) > quota
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::StorageFull,
                format!(
                    "wal quota exceeded by durability marker: cur={pre_marker_offset} + {} > quota={quota}",
                    entry.len()
                ),
            ));
        }
        if let Some(min_free) = state.limits.wal_min_free_bytes
            && let Some(wal_path) = state.wal_path.as_deref()
        {
            let free = wal_volume_free_bytes(wal_path)?;
            if free < min_free {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::StorageFull,
                    format!(
                        "WAL volume below water-mark for durability marker: free={free} < SPG_WAL_MIN_FREE_BYTES={min_free}"
                    ),
                ));
            }
        }
        wal.write_all(&entry)?;
        pre_marker_offset
        // wal mutex guard dropped here
    };
    // Fsync without holding the wal mutex. Both `wal_sync_clone`
    // and `wal` reference the same kernel file; `sync_data` only
    // needs `&File`. Client INSERTs can re-acquire the mutex
    // freely during the fsync.
    WAL_FSYNCS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    if let Some(sync_handle) = state.wal_sync_clone.as_ref() {
        wal_sync(sync_handle)?;
    } else {
        // Fallback: `try_clone` failed at startup (very rare). Take
        // the mutex briefly to sync — the slow case, but at least
        // correct.
        let wal = wal_mutex
            .lock()
            .map_err(|_| std::io::Error::other("wal mutex poisoned"))?;
        wal_sync(&wal)?;
    }
    Ok(pre_marker_offset)
}

/// v4.42 — concatenate already-framed v3 records for a group of
/// auto-commit writes and append them in **one** `write_all` +
/// **one** `sync_data`. The leader calls this between the prepare
/// and install phases of `run_leader_commit_round` so all writers
/// in the group share a single fsync. `entries` is the framed
/// payload sequence (each item is what `encode_wal_v3_record`
/// produced for one task's SQL). Quota / disk-water-mark checks
/// happen once for the whole batch, so a leader either commits
/// the whole group or rolls back every member — same fan-out
/// invariant `chaos_disk_full_multi_client_group_rollback_all_writers`
/// pins.
/// v7.37.15 (r172) — effective `synchronous_commit` for the current
/// session. PG semantics: the session GUC (set via SQL
/// `SET synchronous_commit = off`) decides whether this commit waits
/// for fsync; `SPG_SYNCHRONOUS_COMMIT` stays the process default when
/// no session value is set. The engine's session_params store is
/// process-wide on the server (single shared engine — the same shape
/// `statement_timeout` already has), so a SET on one connection
/// applies server-wide until RESET.
///
/// Takes a short engine **read** lock — callers must not hold the
/// engine write lock (the dispatch loop captures this before taking
/// it, same as `session_statement_timeout_ms`).
/// v7.39 (round 190, D13) — client-path fsync with chaos injection.
/// `SPG_FAIL_FSYNC_AT=K` makes the K-th call (1-based, process-wide)
/// fail once with an injected EIO; unset costs one branch.
/// v7.39 (round 438) — always-on WAL commit-path counters.
///
/// The 2026-07-25 audit found SPGS losing 20x to PG18 on `tx_batch_100`
/// (BEGIN + 100 INSERTs + COMMIT), with the same wall time as 100 autocommit
/// INSERTs — i.e. the transaction bought no fsync amortisation, even though
/// `persist_wire_write` is written to skip the fsync while a transaction is
/// open. Wall-clock arithmetic can only *suggest* how many fsyncs ran; these
/// count them. Free when nothing reads them (two relaxed adds on a path that
/// is about to do disk I/O), so they are always on rather than env-gated —
/// a counter you have to remember to enable is a counter you do not have
/// when you need it.
/// v7.39 (round 476) — the WAL file, so `pg_current_wal_lsn()` can report
/// its byte position.
pub(crate) static WAL_PATH: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();

/// The WAL byte position, as `Engine::set_wal_lsn_fn` wants it.
///
/// Reads the file rather than keeping a counter. There are three append
/// paths (`append_wal`, the v3 group append, and the durability marker) and
/// the first cut incremented a counter in only one of them — the LSN then
/// froze the moment traffic took the group-commit route, which is the route
/// the panel uses. The file's length cannot be forgotten by a new path.
/// `pg_current_wal_lsn()` is a monitoring call, not a hot path, so the stat
/// is affordable; 0 when there is no WAL, which is the honest answer there.
pub(crate) fn wal_lsn_position() -> u64 {
    WAL_PATH
        .get()
        .and_then(|p| std::fs::metadata(p).ok())
        .map_or(0, |m| m.len())
}

pub(crate) static WAL_APPENDS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
pub(crate) static WAL_FSYNCS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// v7.39 (round 438) — one stderr line per fsync when `SPG_WAL_TRACE=1`.
/// Off by default (one relaxed load of a `OnceLock<bool>`), so a perf run
/// can count fsyncs exactly instead of inferring the count from wall time.
pub(crate) fn trace_wal_counters(site: &str) {
    // `SPG_WAL_TRACE=1` → stderr. `SPG_WAL_TRACE=<path>` → append to that
    // file, which is what a bench harness needs: it owns the child's stderr
    // for its own handshake and would otherwise drop (or block on) the trace.
    static DEST: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    let Some(dest) = DEST
        .get_or_init(|| std::env::var("SPG_WAL_TRACE").ok().filter(|v| v != "0"))
        .as_deref()
    else {
        return;
    };
    let (a, f) = wal_counters();
    // v7.38.2 (R2 S3.1) — the same trace line carries the lock-wait
    // counters so one grep prices fsyncs AND poll-retries per window.
    let r = crate::LOCK_WAIT_RETRIES.load(std::sync::atomic::Ordering::Relaxed);
    let s = crate::LOCK_WAIT_SLEEP_US.load(std::sync::atomic::Ordering::Relaxed);
    let line =
        format!("[wal-trace] {site} appends={a} fsyncs={f} lock_retries={r} lock_sleep_us={s}\n");
    if dest == "1" {
        eprint!("{line}");
    } else if let Ok(mut fh) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dest)
    {
        use std::io::Write as _;
        let _ = fh.write_all(line.as_bytes());
    }
}

/// `(appends, fsyncs)` since process start.
#[must_use]
pub(crate) fn wal_counters() -> (u64, u64) {
    (
        WAL_APPENDS.load(std::sync::atomic::Ordering::Relaxed),
        WAL_FSYNCS.load(std::sync::atomic::Ordering::Relaxed),
    )
}

/// v7.39 (round 702-SW) — the WAL's durability primitive, in one place.
///
/// On macOS, `File::sync_data` asks for `F_FULLFSYNC` — a full barrier to
/// the drive's media, measured at **4.08 ms** per call on the testbed. The
/// same box answers `F_BARRIERFSYNC` in **0.37 ms**: writes are ordered and
/// flushed with a barrier token, without waiting for the media confirmation.
/// That one primitive was the whole of the write-path red line: 100
/// autocommit INSERTs cost 511 ms here against PG18's 145 over the same
/// wire, and 100 × (4.08 − 0.37) is the difference.
///
/// Choosing the cheaper barrier is not a durability retreat relative to
/// what SPG competes with — it is the same platform-default philosophy PG
/// itself applies. PG's `wal_sync_method` on macOS defaults to plain
/// `fdatasync`, which forces no drive barrier AT ALL (its `fsync_writethrough`
/// = F_FULLFSYNC exists and is not the default); SQLite's WAL makes the
/// same choice. F_BARRIERFSYNC is *stronger* than both defaults.
///
/// `SPG_WAL_FULLFSYNC=1` opts back into F_FULLFSYNC for deployments that
/// want the media confirmation — the strict direction, so an env var is the
/// right channel. The fast path is the default, per the sealed-fix rule.
/// A failing fcntl (filesystem without barrier support) falls back to
/// `sync_data` on the spot.
// The one unsafe is the `fcntl` FFI call itself — same class as the
// `statvfs` in `wal_volume_free_bytes`, and just as self-contained: a raw
// fd in, an int out, no memory crosses the boundary.
#[allow(unsafe_code)]
pub(crate) fn wal_sync(f: &std::fs::File) -> std::io::Result<()> {
    #[cfg(target_os = "macos")]
    {
        static FULL: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        let force_full =
            // v7.38.18 (G5) — one reading for every boolean switch:
            // `false` used to mean ON here, because only `0` was off.
            *FULL.get_or_init(|| {
                crate::wal_fullfsync_from(std::env::var(crate::WAL_FULLFSYNC_ENV).ok().as_deref())
            });
        if !force_full {
            use std::os::fd::AsRawFd as _;
            // libc has no F_BARRIERFSYNC constant; Apple's fcntl.h says 85.
            const F_BARRIERFSYNC: libc::c_int = 85;
            let rc = unsafe { libc::fcntl(f.as_raw_fd(), F_BARRIERFSYNC) };
            if rc != -1 {
                return Ok(());
            }
            // Unsupported filesystem — take the full barrier instead.
        }
    }
    f.sync_data()
}

fn client_fsync(state: &ServerState, f: &std::fs::File) -> std::io::Result<()> {
    static FSYNC_CALLS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    WAL_FSYNCS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    trace_wal_counters("client_fsync");
    if let Some(k) = state.chaos.fail_fsync_at {
        let n = FSYNC_CALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
        if n == k {
            return Err(std::io::Error::other(
                "chaos: injected fsync failure (SPG_FAIL_FSYNC_AT)",
            ));
        }
    }
    wal_sync(f)
}

pub(crate) fn session_sync_commit(state: &ServerState) -> bool {
    let session = state
        .engine
        .read()
        .ok()
        .and_then(|e| e.session_param("synchronous_commit").map(str::to_string));
    match session {
        Some(v) => !matches!(v.to_ascii_lowercase().as_str(), "off" | "false" | "0"),
        None => !synchronous_commit_disabled(),
    }
}

pub(crate) fn append_wal_v3_group(
    state: &ServerState,
    entries: &[Vec<u8>],
    sync: bool,
) -> std::io::Result<()> {
    let Some(wal) = state.wal.as_ref() else {
        return Ok(());
    };
    if entries.is_empty() {
        return Ok(());
    }
    let total: usize = entries.iter().map(Vec::len).sum();
    let mut batched = Vec::with_capacity(total);
    for e in entries {
        batched.extend_from_slice(e);
    }
    let mut f = wal
        .lock()
        .map_err(|_| std::io::Error::other("wal mutex poisoned"))?;
    if let Some(quota) = state.chaos.wal_quota_bytes {
        let current = f.metadata().map_or(0, |m| m.len());
        if current.saturating_add(batched.len() as u64) > quota {
            return Err(std::io::Error::new(
                std::io::ErrorKind::StorageFull,
                format!(
                    "wal quota exceeded: cur={current} + {} > quota={quota} (SPG_FAIL_WAL_QUOTA_BYTES)",
                    batched.len()
                ),
            ));
        }
    }
    if let Some(min_free) = state.limits.wal_min_free_bytes
        && let Some(wal_path) = state.wal_path.as_deref()
    {
        let free = wal_volume_free_bytes(wal_path)?;
        if free < min_free {
            return Err(std::io::Error::new(
                std::io::ErrorKind::StorageFull,
                format!(
                    "WAL volume below water-mark: free={free} < SPG_WAL_MIN_FREE_BYTES={min_free}"
                ),
            ));
        }
    }
    // v7.39 (round 190, D13) — remember the pre-append length so a
    // failed fsync can truncate its own bytes back off. Pre-r190 the
    // in-memory rollback left the group's bytes in the file, the NEXT
    // successful fsync made them durable, and replay resurrected the
    // rolled-back statements (silent-wrong, r190 chaos pin).
    let pre_append_len = f.metadata()?.len();
    f.write_all(&batched)?;
    // v5.4.2 — in async-commit mode the flusher thread is
    // responsible for `sync_data`; the client's CC may return
    // before the bytes reach disk. v4.42 group-commit semantics
    // are preserved exactly in sync mode (the default).
    //
    // v7.37.15 (r172) — `sync` is resolved by the caller from the
    // session GUC + env default (`session_sync_commit`), so SQL
    // `SET synchronous_commit = off` skips the fsync exactly like
    // the env knob. Skipping arms the flusher via
    // `async_commit_dirty` so durability markers bound the loss
    // window (PG wal-writer semantics).
    if sync {
        if let Err(e) = client_fsync(state, &f) {
            // Best-effort byte rollback under the still-held wal
            // mutex; if the truncate ALSO fails the file may retain
            // un-fsynced garbage — recovery's torn-tail handling
            // drops a partial tail, and a full D13 fsync-poison
            // (PG PANIC semantics) stays a recorded residual.
            let _ = f.set_len(pre_append_len);
            return Err(e);
        }
    } else {
        state
            .async_commit_dirty
            .store(true, std::sync::atomic::Ordering::Release);
    }
    // v6.10.6 — best-effort WAL tee. When `SPG_WAL_TEE_PATH` is
    // set, append the same group bytes to the tee path so an
    // offline observer can mirror the WAL stream without
    // intercepting the primary durability path. Failures are
    // logged + swallowed: the primary WAL append has already
    // succeeded; a tee outage must not roll back committed
    // state.
    if let Some(tee_path) = wal_tee_path() {
        if let Err(e) = append_to_tee(tee_path, &batched) {
            eprintln!("spg-server: WAL tee append to {tee_path:?} failed: {e}");
        }
    }
    Ok(())
}

/// v6.10.6 — read `SPG_WAL_TEE_PATH` once + cache. Returns
/// `Some(&str)` to a 'static path string when the env is set,
/// `None` otherwise.
pub(crate) fn wal_tee_path() -> Option<&'static str> {
    static CACHED: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    CACHED
        .get_or_init(|| env::var("SPG_WAL_TEE_PATH").ok().filter(|s| !s.is_empty()))
        .as_deref()
}

/// v6.10.6 — append `bytes` to the tee file. Opens with O_APPEND
/// + creates if missing. Does NOT fsync — the tee is a
/// best-effort mirror, not a durability surface. (Operators
/// fronting the tee with a remote-mounted filesystem get
/// "sync-after-batch" semantics from the OS's page cache
/// flush.)
pub(crate) fn append_to_tee(path: &str, bytes: &[u8]) -> std::io::Result<()> {
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    f.write_all(bytes)
}

/// v4.42 — `io::Error` is intentionally not `Clone` (the OS error
/// inside it isn't reproducible by value-copy alone), but the
/// commit-barrier leader has to fan one fsync outcome out to N
/// `CommitTask::ack` channels. Reconstruct with the same
/// `ErrorKind` and the original `Display` representation. The
/// chaos test asserts the *kind* is `StorageFull` (so quota-
/// exceeded fan-out is recognisable as ENOSPC by every writer),
/// which this preserves.
pub(crate) fn clone_io_err(e: &std::io::Error) -> std::io::Error {
    std::io::Error::new(e.kind(), e.to_string())
}

/// v4.42 — read `SPG_COMMIT_GROUP_MAX` at queue-pull time so the
/// bench knob can change between connections without a restart.
/// Unset / unparseable / zero → fall back to
/// `DEFAULT_COMMIT_GROUP_MAX`.
pub(crate) fn commit_group_max() -> usize {
    parse_env_usize("SPG_COMMIT_GROUP_MAX").unwrap_or(DEFAULT_COMMIT_GROUP_MAX)
}

/// v4.42 — micro-spin window the leader gives concurrent writers
/// to populate the queue before forming a group. Read fresh from
/// the env on every leader iteration so a benchmark can flip it
/// without a server restart. Mirrors PG's `commit_delay`: zero
/// means "ship what's already queued" (the honest single-client
/// default — group of 1 always, no latency tax), positive N means
/// "spin-wait up to N µs for the queue to fill toward
/// `SPG_COMMIT_GROUP_MAX`". The sweep + multi-client SLO smoke
/// set this to ~200 µs because on macOS APFS a single fsync is
/// ~milliseconds — a 200 µs spin is well under that cost and
/// pays for itself by letting 4-16 writers share one fsync.
pub(crate) fn commit_delay_us() -> u64 {
    parse_env_u64("SPG_COMMIT_DELAY_US").unwrap_or(0)
}

/// v7.39 (round 741, S15/B4) — the ADAPTIVE coalescing window, in
/// microseconds. PG's `commit_delay` is a hand-tuned constant; this one
/// steers itself by the only signal that matters — did the last group
/// actually coalesce? AIMD: a group of >= 2 doubles the window (from a
/// 25 µs floor) up to 200 µs — half an F_BARRIERFSYNC, so one merged
/// writer already pays for the wait — and a group of 1 halves it back
/// toward zero, which is why a serial single client (whose waits can
/// never see a second task) decays to the zero-latency shape within a
/// few statements. An explicit SPG_COMMIT_DELAY_US pins the window and
/// disables adaptation entirely.
pub(crate) static ADAPTIVE_COMMIT_WINDOW_US: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// v7.39 (round 741) — ground truth for the S15 bench: how many tasks
/// each finished group carried, split into "flushed alone" vs
/// "coalesced", plus the coalesced task total. (The S14 lesson: green
/// output cannot distinguish an engaged optimisation from a silent
/// fallback; counters can.)
pub(crate) static COMMIT_GROUPS_SOLO: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);
pub(crate) static COMMIT_GROUPS_COALESCED: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);
pub(crate) static COMMIT_TASKS_COALESCED: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// The window the leader actually uses this round.
fn effective_commit_delay_us() -> u64 {
    let explicit = parse_env_u64("SPG_COMMIT_DELAY_US");
    match explicit {
        Some(v) => v,
        None => ADAPTIVE_COMMIT_WINDOW_US.load(std::sync::atomic::Ordering::Relaxed),
    }
}

/// AIMD update after a group of `n` tasks ships. No-op when the
/// operator pinned the window.
fn adapt_commit_window(n: usize) {
    use std::sync::atomic::Ordering;
    if parse_env_u64("SPG_COMMIT_DELAY_US").is_some() {
        return;
    }
    let cur = ADAPTIVE_COMMIT_WINDOW_US.load(Ordering::Relaxed);
    let next = if n >= 2 {
        (cur.max(25)).saturating_mul(2).min(200)
    } else {
        cur / 2
    };
    ADAPTIVE_COMMIT_WINDOW_US.store(next, Ordering::Relaxed);
}

/// v4.42 — push a `CommitTask` onto the commit-barrier queue and
/// decide whether the caller becomes the leader. Returns `true`
/// iff the latching `leader_active` flag flipped from `false` to
/// `true` on this push (= caller is now responsible for driving
/// `run_leader_commit_round`). Returns `false` if another writer
/// is already leading; the caller then waits on its `ack` channel
/// for that leader to commit (or roll back) its task.
pub(crate) fn enqueue_commit_task(state: &ServerState, task: CommitTask) -> bool {
    let mut q = state
        .commit_queue
        .lock()
        .expect("commit queue mutex poisoned");
    q.pending.push_back(task);
    if q.leader_active {
        false
    } else {
        q.leader_active = true;
        true
    }
}

/// v4.42 — leader loop. Runs while `leader_active` is true and
/// pulls one *group* per iteration (up to `commit_group_max()`
/// tasks). Each iteration runs the classic group commit shape,
/// but **sequentially** under one engine write lock so per-task
/// mutations accumulate into shared catalog state (the previous
/// two-phase prepare/install design lost rows: each task's BEGIN
/// cloned the *same* pre-group catalog into its slot, so when
/// COMMIT moved each slot's catalog over `self.catalog` only the
/// last task's slot survived).
///
/// 1. **Snapshot pre-image** — `engine.catalog().clone()`. After
///    the v4.39/v4.40 persistent migration this is an O(1)
///    Arc bump, so the pre-image carries no per-row cost.
///
/// 2. **Sequential prepare + in-memory commit** — for each task:
///    `alloc_tx_id` → `BEGIN` → `execute_in(sql)` → encode v3
///    WAL bytes → `COMMIT` (merges the slot's catalog over
///    `self.catalog`, so the next task's BEGIN sees this task's
///    row). Tasks that fail any step are `ROLLBACK`-ed in
///    isolation and acked with their own error; surviving tasks
///    collect into a `prepared` list keyed by `wal_bytes`.
///    Engine lock released.
///
/// 3. **Batched fsync barrier** — concat survivors' framed v3
///    bytes; one `write_all` + one `sync_data` under the WAL
///    mutex (`append_wal_v3_group`). Quota / disk-water-mark
///    checks happen once for the whole batch — if the batch
///    doesn't fit, every survivor in the group is rolled back
///    together (the multi-client ENOSPC fan-out invariant
///    `chaos_disk_full_multi_client_group_rollback_all_writers`
///    pins).
///
/// 4. **Fsync-fail rollback** — if fsync returned `Err`,
///    re-acquire `engine.write()` and `replace_catalog(pre_image)`
///    to undo every in-memory commit from step 2 at once. Ack
///    each survivor with `{ Ok(exec_result), Err(wal_outcome) }`
///    so dispatch's WAL-error short-circuit reports the failure
///    to the client (and the in-memory state matches the durable
///    state — no phantom rows survive).
///
/// 5. **Ack survivors** — every prepared task is acked here
///    whether fsync succeeded or failed; the dispatch thread's
///    `recv` is the durability contract.
///
/// Rolling drain: after step 5 (or whenever the queue is empty),
/// re-check `state.commit_queue.pending` under the mutex; if
/// new tasks arrived during fsync, loop and form the next group;
/// if not, flip `leader_active = false` and return.
///
/// The function naturally runs >100 lines because group commit's
/// five stages (drain → prepare/in-memory-commit → batched fsync
/// → rollback-on-fail → ack) all touch shared state under the
/// same engine write lock and the same loop iteration; splitting
/// them into helpers would only scatter the control flow.
#[allow(clippy::too_many_lines)]
pub(crate) fn run_leader_commit_round(state: &ServerState) {
    // Per-task scratch carried through the leader's pipeline:
    // declared at module-scope shape so clippy doesn't trip on
    // items-after-statements inside the loop body.
    struct Prepared {
        task: CommitTask,
        result: QueryResult,
        wal_bytes: Vec<u8>,
    }
    let group_max = commit_group_max();
    loop {
        // v7.39 (round 741, S15/B4) — re-read per round: the window
        // adapts between rounds.
        let delay_us = effective_commit_delay_us();
        // ----- 1. Pull one group under the queue lock -----
        //
        // First check non-blocking. If pending is already full or
        // delay_us = 0 (honest single-client default), batch what's
        // there and run the group immediately — group of 1 in the
        // common single-client case, exactly matches the v4.41.1
        // latency shape with no extra wait.
        //
        // If pending is short and delay_us > 0, spin-yield up to
        // `delay_us` microseconds for concurrent writers to push
        // more tasks. Spinning (not sleeping) keeps the wakeup
        // latency sub-microsecond — critical on macOS APFS where
        // a single fsync is multiple milliseconds: a 200 µs spin
        // is cheap insurance to coalesce 4-16 writers into one
        // fsync.
        let group: Vec<CommitTask> = {
            let mut q = state
                .commit_queue
                .lock()
                .expect("commit queue mutex poisoned");
            if delay_us > 0 && q.pending.len() < group_max {
                let deadline = Instant::now() + Duration::from_micros(delay_us);
                while q.pending.len() < group_max && Instant::now() < deadline {
                    drop(q);
                    thread::yield_now();
                    q = state
                        .commit_queue
                        .lock()
                        .expect("commit queue mutex poisoned");
                }
            }
            if q.pending.is_empty() {
                // No more work: drop the leader baton inside the
                // critical section so the next push can claim it
                // atomically.
                q.leader_active = false;
                return;
            }
            let take = q.pending.len().min(group_max);
            q.pending.drain(..take).collect()
        };
        // v7.39 (round 741) — steer the window and record ground truth.
        {
            use std::sync::atomic::Ordering;
            if group.len() >= 2 {
                COMMIT_GROUPS_COALESCED.fetch_add(1, Ordering::Relaxed);
                COMMIT_TASKS_COALESCED.fetch_add(group.len() as u64, Ordering::Relaxed);
            } else {
                COMMIT_GROUPS_SOLO.fetch_add(1, Ordering::Relaxed);
            }
            adapt_commit_window(group.len());
        }

        // ----- 2. Sequential prepare + in-memory commit -----
        //
        // v7.39 (round 194) — each task executes as a plain
        // AUTOCOMMIT statement (`execute_in` with no BEGIN/COMMIT).
        // The v4.42 BEGIN + sql + COMMIT wrap routed every queued
        // write through the engine's TRANSACTION path — shadow
        // catalog + RC rebase — which replays the statement's whole
        // write-set at commit: O(rows) on top of the write itself.
        // Once r178 routed pgwire's bare DML through this queue, a
        // 20k-row UPDATE went 25 ms → 1.9 s and a 1k-row
        // delete+reinsert 2 ms → 205 ms on the wire panel. A single
        // autocommit statement is already atomic (the engine rolls a
        // failed statement back itself), so per-task isolation needs
        // no tx wrap, and the fsync-failure path still restores the
        // whole group via `pre_image` + `replace_catalog`. Their WAL
        // bytes are concatenated and batched-fsync'd in step 3.
        let mut prepared: Vec<Prepared> = Vec::with_capacity(group.len());
        let pre_image: Option<spg_storage::Catalog> = {
            let Ok(mut engine) = state.engine.write() else {
                // Engine lock poisoned — fatal, server can't make
                // progress. Drop the group (auto-closes every
                // task's ack channel; dispatch threads see
                // `RecvError` and surface a clean io error to
                // their clients) and release the leader baton so
                // future arrivals don't deadlock waiting for a
                // dead leader.
                drop(group);
                if let Ok(mut q) = state.commit_queue.lock() {
                    q.leader_active = false;
                }
                return;
            };
            // O(1) Arc-bump clone (v4.39/v4.40 persistent
            // backing). Stays cheap regardless of row count.
            let pre = engine.catalog().clone();
            for task in group {
                // r194 — encode the WAL frame BEFORE executing: it
                // depends only on the SQL text, and an autocommit
                // statement can't be rolled back individually after
                // the fact (the old tx wrap could). A failed encode
                // now skips the task with zero side effects.
                let wal_bytes = match encode_wal_auto_commit_sql_metrics(&task.sql, &state.metrics)
                {
                    Ok(b) => b,
                    Err(e) => {
                        let _ = task.ack.send(CommitResult {
                            result: Err(spg_engine::EngineError::Unsupported(format!(
                                "WAL encode failed: {e}"
                            ))),
                            wal_outcome: Err(e),
                            audited: false,
                        });
                        continue;
                    }
                };
                let tx_id = engine.alloc_tx_id();
                let exec_res = engine.execute_in_with_cancel(
                    &task.sql,
                    tx_id,
                    spg_engine::CancelToken::from_flag(&task.cancel_flag),
                );
                // v7.39 (round 178) — a DML with RETURNING answers
                // `Rows`, not `CommandOk`. Pre-r178 the leader treated
                // ANY non-CommandOk as a failed slot and ROLLBACK-ed it
                // while the dispatch thread still returned the rows to
                // the client: a silent-wrong (acked write, data gone).
                // Rows from a DML-shaped statement now commit + WAL
                // like any other write; Rows from a non-DML text (a
                // read that slipped past sql_is_read_only) keep the
                // old rollback path, which is harmless for a read.
                let commit_worthy = matches!(exec_res, Ok(QueryResult::CommandOk { .. }))
                    || (matches!(exec_res, Ok(QueryResult::Rows { .. }))
                        && crate::sql_is_dmlish(&task.sql));
                if !commit_worthy {
                    // SQL itself failed (parse / type / cancel) — a
                    // failed autocommit statement rolled itself back
                    // in the engine (r194: no tx wrap to discard);
                    // ack with the engine error.
                    let _ = tx_id;
                    let _ = task.ack.send(CommitResult {
                        result: exec_res,
                        wal_outcome: Ok(()),
                        audited: false,
                    });
                    continue;
                }
                // r194 — no COMMIT step: the autocommit statement
                // already applied to `engine.catalog` (the next
                // task sees its effects directly).
                prepared.push(Prepared {
                    task,
                    result: exec_res.unwrap(),
                    wal_bytes,
                });
            }
            // Hand back the pre-image only if we actually
            // mutated state; that's the only case where a fsync
            // failure would need to roll back.
            if prepared.is_empty() { None } else { Some(pre) }
        }; // engine write lock released here

        if prepared.is_empty() {
            // Whole group failed prepare; nothing to fsync, no
            // rollback needed. Loop to pull the next group.
            continue;
        }

        // ----- 2.5 Audit barrier (r1055 P1 fix, design D29) -----
        //
        // ack ⇒ durable AND audited; error ⇒ NO effect. The audit
        // entry lands BEFORE any WAL byte: on audit failure the
        // group rolls back whole (the same pre_image mechanism the
        // fsync-fail path below uses) and the WAL is untouched, so
        // the client's error truthfully means "nothing happened".
        //
        // The old order appended audit AFTER the durable commit, so
        // an audit failure produced an error for a statement that
        // had taken effect — a retrying client double-writes. The
        // S3.4 fault test's first run caught it.
        let leader_audits = state.audit_path.is_some();
        if leader_audits {
            let mut audit_err: Option<std::io::Error> = None;
            let mut appended_any = false;
            for p in &prepared {
                if !matches!(
                    p.result,
                    QueryResult::CommandOk {
                        modified_catalog: true,
                        ..
                    }
                ) {
                    continue;
                }
                match crate::append_audit_pub(state, &p.task.sql) {
                    Ok(()) => appended_any = true,
                    Err(e) => {
                        audit_err = Some(e);
                        break;
                    }
                }
            }
            if let Some(e) = audit_err {
                if let Some(pre) = pre_image {
                    if let Ok(mut engine) = state.engine.write() {
                        engine.replace_catalog(pre);
                    }
                }
                if appended_any {
                    // The chain already carries entries for statements
                    // this rollback annuls. One honest annul note keeps
                    // the chain verifiable without lying; best-effort —
                    // a persistently broken audit device cannot take it.
                    let _ = crate::append_audit_pub(
                        state,
                        "-- spg: audit annul — preceding group rolled back                          (audit append failure before WAL)",
                    );
                }
                for p in prepared {
                    let _ = p.task.ack.send(CommitResult {
                        result: Ok(p.result),
                        wal_outcome: Err(clone_io_err(&e)),
                        audited: true,
                    });
                }
                continue;
            }
        }

        // ----- 3. Batched fsync barrier -----
        // v7.37.15 (r172) — a group syncs iff any member's session
        // wants synchronous_commit. An all-async group appends
        // without fsync (the flusher bounds the loss window); a
        // mixed group's single fsync covers the async members for
        // free, same as PG's group flush.
        let entries: Vec<Vec<u8>> = prepared.iter().map(|p| p.wal_bytes.clone()).collect();
        let want_sync = prepared.iter().any(|p| p.task.sync_commit);
        let wal_outcome: std::io::Result<()> = append_wal_v3_group(state, &entries, want_sync);

        // ----- 4. Fsync-fail rollback -----
        if wal_outcome.is_err()
            && let Some(pre) = pre_image
        {
            // r1055 (D29) — the audit barrier above already recorded
            // this group's catalog statements; the rollback annuls
            // them, and the chain says so. Best-effort by nature.
            if leader_audits {
                let _ = crate::append_audit_pub(
                    state,
                    "-- spg: audit annul — preceding group rolled back (WAL append failure)",
                );
            }
            if let Ok(mut engine) = state.engine.write() {
                engine.replace_catalog(pre);
            } else {
                // Poisoned mid-rollback: every survivor's ack
                // channel will surface the WAL error anyway, but
                // the catalog now diverges from the durable WAL.
                // Leader can't fix that; bail and let the next
                // bootup's WAL replay reconverge.
                drop(prepared);
                if let Ok(mut q) = state.commit_queue.lock() {
                    q.leader_active = false;
                }
                return;
            }
        }

        // ----- 5. Ack survivors -----
        // Dispatch checks `wal_outcome` first (the v4.41.1
        // "WAL append failed: ..." error shape lives in that
        // branch), so even when the in-memory exec succeeded but
        // fsync failed, the client sees the WAL error and
        // recovers to a state consistent with the durable WAL.
        //
        // v6.10.0 — also fan out each successfully-committed SQL
        // to the pubsub side-channel. Fires only when WAL fsync
        // succeeded (no point publishing a record that hasn't
        // landed on disk).
        let wal_ok = wal_outcome.is_ok();
        for p in prepared {
            let cloned_wal = match &wal_outcome {
                Ok(()) => Ok(()),
                Err(e) => Err(clone_io_err(e)),
            };
            if wal_ok {
                pubsub::publish_sql(&p.task.sql);
            }
            let _ = p.task.ack.send(CommitResult {
                result: Ok(p.result),
                wal_outcome: cloned_wal,
                audited: leader_audits,
            });
        }
        // loop back to pull the next group (rolling drain).
    }
}

/// v7.39 (round 250) — one explicit fsync of the WAL file, used by the
/// COPY `ON_ERROR SET_NULL` path: its rows append per-row without sync
/// (the extension's contract is per-row autocommit, not one
/// transaction), so the COPY's CommandComplete anchors durability with
/// a single fsync here instead.
pub(crate) fn wal_fsync_now(state: &ServerState) -> std::io::Result<()> {
    let Some(wal) = state.wal.as_ref() else {
        return Ok(());
    };
    let f = wal
        .lock()
        .map_err(|_| std::io::Error::other("wal mutex poisoned"))?;
    client_fsync(state, &f)
}

pub(crate) fn append_wal(state: &ServerState, sql: &str, sync: bool) -> std::io::Result<()> {
    let Some(wal) = state.wal.as_ref() else {
        return Ok(());
    };
    WAL_APPENDS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let entry = encode_wal_record(sql)?;
    let mut f = wal
        .lock()
        .map_err(|_| std::io::Error::other("wal mutex poisoned"))?;
    // v4.29 chaos: simulated disk-full. Reject the append before
    // touching the OS so committed state is unaffected. Returned
    // error propagates as a clean ErrorResponse to the client.
    if let Some(quota) = state.chaos.wal_quota_bytes {
        let current = f.metadata().map_or(0, |m| m.len());
        if current.saturating_add(entry.len() as u64) > quota {
            return Err(std::io::Error::new(
                std::io::ErrorKind::StorageFull,
                format!(
                    "wal quota exceeded: cur={current} + {} > quota={quota} (SPG_FAIL_WAL_QUOTA_BYTES)",
                    entry.len()
                ),
            ));
        }
    }
    // v4.33 disk water-mark: when `SPG_WAL_MIN_FREE_BYTES` is set,
    // call statvfs on the WAL volume and refuse the append if free
    // space is below the threshold. Writes return StorageFull; reads
    // continue (this path is write-only). Defaults off — when unset,
    // `state.limits.wal_min_free_bytes` is None and we skip the
    // syscall entirely.
    if let Some(min_free) = state.limits.wal_min_free_bytes
        && let Some(wal_path) = state.wal_path.as_deref()
    {
        let free = wal_volume_free_bytes(wal_path)?;
        if free < min_free {
            return Err(std::io::Error::new(
                std::io::ErrorKind::StorageFull,
                format!(
                    "WAL volume below water-mark: free={free} < SPG_WAL_MIN_FREE_BYTES={min_free}"
                ),
            ));
        }
    }
    // r190 (D13) — same failed-fsync byte rollback as the group path.
    let pre_append_len = f.metadata()?.len();
    f.write_all(&entry)?;
    // v5.4.2 — async-commit mode opts out of the per-write
    // `sync_data`; durability rides on the flusher thread's
    // periodic `durability_checkpoint` markers instead.
    //
    // v7.37.15 (r172) — `sync` is caller-resolved from the session
    // GUC + env default (`session_sync_commit`); this function
    // can't read it itself because the main-loop non-wrap caller
    // holds the engine write lock. Skipping arms the flusher.
    if sync {
        if let Err(e) = client_fsync(state, &f) {
            let _ = f.set_len(pre_append_len);
            return Err(e);
        }
    } else {
        state
            .async_commit_dirty
            .store(true, std::sync::atomic::Ordering::Release);
    }
    Ok(())
}

/// v4.33: free bytes on the filesystem that owns `path`, via
/// `statvfs(2)`. macOS and Linux both expose `statvfs` with
/// compatible field semantics (`f_bavail` × `f_frsize`).
/// `f_bavail` (vs `f_bfree`) excludes blocks reserved for the
/// superuser, which is what an unprivileged write actually has
/// access to — the same number `df` shows in its "Avail" column.
///
/// The `as u64` casts are widening on every supported platform
/// (`fsblkcnt_t`/`c_ulong` are u32 on apple, u64 on linux); pin
/// the lossless-cast lint locally so the same source compiles
/// cleanly on both without per-cfg branches.
#[allow(unsafe_code, clippy::cast_lossless, clippy::useless_conversion)]
pub(crate) fn wal_volume_free_bytes(path: &Path) -> std::io::Result<u64> {
    use std::os::unix::ffi::OsStrExt;
    let bytes = path.as_os_str().as_bytes();
    let mut c_path = Vec::with_capacity(bytes.len() + 1);
    c_path.extend_from_slice(bytes);
    c_path.push(0);
    // SAFETY: `statvfs` reads a NUL-terminated path and writes into
    // the provided buffer. We give it both. The buffer is initialized
    // by the call (the kernel writes every field on success); on
    // failure we return early via the errno check before reading any
    // field. macOS + Linux `libc::statvfs` signatures match.
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::statvfs(c_path.as_ptr().cast(), &raw mut stat) };
    if rc != 0 {
        return Err(std::io::Error::last_os_error());
    }
    let bavail = stat.f_bavail as u64;
    let frsize = stat.f_frsize as u64;
    Ok(bavail.saturating_mul(frsize))
}

/// Replay WAL bytes onto `engine`. Returns the number of entries applied.
/// Handles all three record formats:
///   v1 (≤ v4.36): `[u32 len][len bytes]` — no CRC. bit 31 = 0.
///   v2 (v4.37+):  `[u32 (len | 0x8000_0000)][u32 crc32][len bytes]`.
///                 bit 31 = 1, bit 30 = 0.
///   v3 (v4.41+):  `[u32 (len | 0xC000_0000)][u32 crc32][1 byte type][len bytes payload]`.
///                 bit 31 = 1, bit 30 = 1. The CRC covers
///                 `[type byte || payload]`. Unknown type byte is
///                 fatal — never silently skipped.
/// The format is detected per-record by the sentinel bits; a WAL
/// file that interleaves multiple versions (mid-upgrade) still
/// replays correctly. A truncated trailing entry (e.g. crash mid-
/// append) is dropped with a warning to stderr. Non-truncation
/// errors — engine rejected SQL, bad UTF-8, CRC mismatch, unknown
/// v3 type — are fatal: the operator must inspect.
///
/// v5.4: type-tag dispatch is delegated to `dispatch_v3_record` so
/// new v3 kinds (like `durability_checkpoint`) extend the namespace
/// without inflating this function past the per-function line
/// budget.
/// v7.30.1 (mailrs round-24 ask 2) — run one replayed statement;
/// an engine REJECT is quarantined (loud stderr line, replay
/// continues) instead of failing the boot. "One statement failed
/// to replay" ≠ "the WAL is corrupt" — framing and CRC damage
/// still error out in the callers.
pub(crate) fn replay_execute_quarantining(engine: &mut Engine, sql: &str, frame_off: usize) {
    if let Err(e) = engine.execute(sql) {
        eprintln!(
            "spg-server: WAL replay QUARANTINED statement at offset {frame_off} \
             (boot continues): {sql:?} rejected: {e:?}"
        );
    }
}

pub(crate) fn dispatch_v3_record(
    tag: u8,
    payload: &[u8],
    frame_off: usize,
    engine: &mut Engine,
) -> std::io::Result<bool> {
    match tag {
        WAL_V3_TYPE_AUTO_COMMIT_SQL => {
            let sql = core::str::from_utf8(payload).map_err(|_| {
                std::io::Error::other("v3 auto_commit_sql payload has non-UTF-8 SQL")
            })?;
            replay_execute_quarantining(engine, sql, frame_off);
            Ok(true)
        }
        WAL_V3_TYPE_COMPRESSED_SQL => {
            // v6.6.1 — `[algo byte][compressed bytes]`. Decompress
            // via LZSS for algo 0x01, route through Engine::execute.
            if payload.is_empty() {
                return Err(std::io::Error::other(format!(
                    "WAL compressed_sql at offset {frame_off}: empty payload"
                )));
            }
            let algo = payload[0];
            let compressed = &payload[1..];
            let raw_bytes = match algo {
                WAL_COMPRESS_ALGO_LZSS => spg_crypto::lzss::decompress(compressed).map_err(|e| {
                    std::io::Error::other(format!(
                        "WAL compressed_sql at offset {frame_off}: LZSS decompress failed: {e:?}"
                    ))
                })?,
                other => {
                    return Err(std::io::Error::other(format!(
                        "WAL compressed_sql at offset {frame_off}: unknown algo byte {other:#04x}"
                    )));
                }
            };
            let sql = core::str::from_utf8(&raw_bytes).map_err(|_| {
                std::io::Error::other(format!(
                    "WAL compressed_sql at offset {frame_off}: decompressed bytes are not valid UTF-8"
                ))
            })?;
            replay_execute_quarantining(engine, sql, frame_off);
            Ok(true)
        }
        WAL_V3_TYPE_DURABILITY_CHECKPOINT => {
            // v5.4.0 — marker is a no-op during replay (engine state
            // isn't mutated); its purpose is to record "every WAL byte
            // before this marker was fsynced by the flusher at write
            // time." Validate payload shape + cross-check the recorded
            // offset against `frame_off`; a mismatch logs a stderr
            // warning (would indicate WAL relocation) but replay keeps
            // going. `Ok(false)` opts the marker out of the user-SQL
            // applied counter.
            if payload.len() != 8 {
                return Err(std::io::Error::other(format!(
                    "WAL durability_checkpoint at offset {frame_off} has {}-byte payload (expected 8)",
                    payload.len()
                )));
            }
            let arr: [u8; 8] = payload.try_into().expect("checked len above");
            let recorded_off = u64::from_le_bytes(arr);
            let frame_off_u64 = frame_off as u64;
            if recorded_off != frame_off_u64 {
                eprintln!(
                    "spg-server: WAL durability_checkpoint at offset {frame_off} carries recorded_off={recorded_off} — possible WAL relocation; treating marker as no-op"
                );
            }
            Ok(false)
        }
        other => Err(std::io::Error::other(format!(
            "WAL v3 unknown type byte {other:#04x} at offset {frame_off} — refusing to replay"
        ))),
    }
}

pub(crate) fn replay_wal_bytes(bytes: &[u8], engine: &mut Engine) -> std::io::Result<usize> {
    // r1055 (7.38 S3.4) — widen the kill window deterministically: a
    // crash-during-recovery test needs recovery to take long enough
    // to be killed IN, and a sleep per applied frame is the honest
    // way to get there without racing. Registered in the sigil index;
    // absent in production environments, zero cost.
    let pause_ms: u64 = std::env::var("SPG_FAULT_RECOVERY_PAUSE_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let mut cur = 0;
    let mut applied = 0usize;
    while cur < bytes.len() {
        if bytes.len() - cur < 4 {
            eprintln!(
                "spg-server: WAL truncated at offset {cur} (need 4-byte length, have {})",
                bytes.len() - cur
            );
            break;
        }
        let frame_off = cur;
        let len_arr: [u8; 4] = bytes[cur..cur + 4].try_into().expect("checked");
        let raw_len = u32::from_le_bytes(len_arr);
        cur += 4;
        let is_v2 = raw_len & WAL_V2_SENTINEL != 0;
        let is_v3 = is_v2 && (raw_len & WAL_V3_FLAG != 0);
        // v3 reuses the v2 sentinel bit + adds bit 30; mask both
        // when extracting the length so v3 lengths read correctly.
        let len_mask = if is_v3 {
            !(WAL_V2_SENTINEL | WAL_V3_FLAG)
        } else {
            !WAL_V2_SENTINEL
        };
        let len = (raw_len & len_mask) as usize;
        let expected_crc = if is_v2 {
            if bytes.len() - cur < 4 {
                eprintln!(
                    "spg-server: v2/v3 WAL truncated at offset {cur} (need 4-byte CRC, have {})",
                    bytes.len() - cur
                );
                break;
            }
            let crc_arr: [u8; 4] = bytes[cur..cur + 4].try_into().expect("checked");
            cur += 4;
            Some(u32::from_le_bytes(crc_arr))
        } else {
            None
        };
        // v3 carries a 1-byte type tag between the CRC and the
        // payload body. Read it here so the rest of the loop sees
        // a uniform `payload` slice.
        let v3_type_tag = if is_v3 {
            if bytes.len() - cur < 1 {
                eprintln!(
                    "spg-server: v3 WAL truncated at offset {cur} (need 1-byte type, have 0)"
                );
                break;
            }
            let t = bytes[cur];
            cur += 1;
            Some(t)
        } else {
            None
        };
        if cur + len > bytes.len() {
            eprintln!("spg-server: WAL entry truncated (payload_len={len}) — dropping tail");
            break;
        }
        let payload = &bytes[cur..cur + len];
        if let Some(expected) = expected_crc {
            let actual = if let Some(tag) = v3_type_tag {
                // CRC covers `[type byte || payload]` in v3 so a
                // flipped type byte fails the check.
                let mut buf = Vec::with_capacity(1 + payload.len());
                buf.push(tag);
                buf.extend_from_slice(payload);
                spg_crypto::crc32::crc32(&buf)
            } else {
                spg_crypto::crc32::crc32(payload)
            };
            if actual != expected {
                return Err(std::io::Error::other(format!(
                    "WAL CRC mismatch at offset {frame_off} (expected={expected:#010x}, computed={actual:#010x}, payload_len={len}) — corruption detected, refusing to replay"
                )));
            }
        }
        // Dispatch by frame version. v1/v2 payload is the SQL text
        // directly; v3 routes on the type tag via `dispatch_v3_record`,
        // which returns `false` only for metadata records (v5.4
        // `durability_checkpoint`) that shouldn't increment the user-
        // SQL `applied` counter.
        let count_as_applied = if let Some(tag) = v3_type_tag {
            dispatch_v3_record(tag, payload, frame_off, engine)?
        } else {
            let sql = core::str::from_utf8(payload)
                .map_err(|_| std::io::Error::other("WAL entry has non-UTF-8 SQL"))?;
            replay_execute_quarantining(engine, sql, frame_off);
            true
        };
        cur += len;
        if count_as_applied {
            applied += 1;
            if pause_ms > 0 {
                std::thread::sleep(core::time::Duration::from_millis(pause_ms));
            }
        }
    }
    Ok(applied)
}

#[cfg(test)]
mod wal_v3_durability_marker_tests {
    use super::{
        Engine, WAL_V2_SENTINEL, WAL_V3_FLAG, WAL_V3_SENTINEL, WAL_V3_TYPE_AUTO_COMMIT_SQL,
        WAL_V3_TYPE_DURABILITY_CHECKPOINT, encode_durability_marker, encode_wal_v3_record,
        replay_wal_bytes,
    };

    #[test]
    fn durability_marker_frame_shape_pins_v3_wire() {
        // Wire-format pin: a marker for byte_offset=0x1234_5678 must
        // produce the v3 envelope `[sentinel|len=8][crc][type=0x02]
        // [u64 LE offset]` — 17 bytes total. Any future change to
        // the frame layout breaks this test, forcing a STABILITY
        // bump conversation.
        let bytes = encode_durability_marker(0x1234_5678).unwrap();
        assert_eq!(bytes.len(), 17, "marker frame must be 17 bytes");
        let raw_len = u32::from_le_bytes(bytes[0..4].try_into().unwrap());
        let len_field = raw_len & !(WAL_V2_SENTINEL | WAL_V3_FLAG);
        assert_eq!(len_field, 8, "marker payload is 8 bytes (the u64 offset)");
        assert_eq!(
            raw_len & WAL_V3_SENTINEL,
            WAL_V3_SENTINEL,
            "marker must carry v3 sentinel bits",
        );
        assert_eq!(
            bytes[8], WAL_V3_TYPE_DURABILITY_CHECKPOINT,
            "type byte must be 0x02",
        );
        let offset = u64::from_le_bytes(bytes[9..17].try_into().unwrap());
        assert_eq!(offset, 0x1234_5678, "payload echoes the offset arg");
    }

    #[test]
    fn replay_skips_durability_markers_and_does_not_increment_applied() {
        // A WAL containing only durability markers replays as a
        // no-op: applied=0, no engine mutation. Three markers at
        // different "recorded offsets" — none match the actual
        // frame_off in this synthetic stream (the first marker is
        // at byte 0, the others follow), so the consistency check
        // hits stderr but replay keeps going.
        let mut stream = Vec::new();
        stream.extend_from_slice(&encode_durability_marker(0).unwrap());
        stream.extend_from_slice(&encode_durability_marker(17).unwrap());
        stream.extend_from_slice(&encode_durability_marker(34).unwrap());
        let mut engine = Engine::new();
        let applied = replay_wal_bytes(&stream, &mut engine).expect("replay must accept markers");
        assert_eq!(applied, 0, "markers do not count as applied records");
    }

    #[test]
    fn replay_mixes_sql_and_markers_advancing_cursor_correctly() {
        // Marker interleaved between two CREATE TABLE statements
        // must not affect cursor accounting: both CREATE TABLEs
        // apply, marker no-ops, applied=2.
        let mut stream = Vec::new();
        let create_a =
            encode_wal_v3_record(WAL_V3_TYPE_AUTO_COMMIT_SQL, b"CREATE TABLE a (id INT)").unwrap();
        let create_b =
            encode_wal_v3_record(WAL_V3_TYPE_AUTO_COMMIT_SQL, b"CREATE TABLE b (id INT)").unwrap();
        let marker_off = create_a.len() as u64;
        let marker = encode_durability_marker(marker_off).unwrap();
        stream.extend_from_slice(&create_a);
        stream.extend_from_slice(&marker);
        stream.extend_from_slice(&create_b);
        let mut engine = Engine::new();
        let applied =
            replay_wal_bytes(&stream, &mut engine).expect("mixed stream must replay cleanly");
        assert_eq!(
            applied, 2,
            "two CREATE TABLEs applied; marker doesn't count"
        );
    }

    #[test]
    fn replay_rejects_marker_with_wrong_payload_length() {
        // A v3 frame typed 0x02 but carrying a payload != 8 bytes is
        // a structural error — replay must surface it, not silently
        // tolerate it. Forge such a frame via `encode_wal_v3_record`
        // with a 4-byte payload.
        let bad =
            encode_wal_v3_record(WAL_V3_TYPE_DURABILITY_CHECKPOINT, &0u32.to_le_bytes()).unwrap();
        let mut engine = Engine::new();
        let err = replay_wal_bytes(&bad, &mut engine).expect_err("4-byte payload must error");
        let msg = err.to_string();
        assert!(
            msg.contains("durability_checkpoint") && msg.contains("4-byte payload"),
            "error message should name the malformed marker: got {msg:?}",
        );
    }
}

#[cfg(test)]
mod wal_sync_tests {
    /// v7.39 (round 702-SW) — the primitive completes and the file's bytes
    /// survive it. What CANNOT be unit-tested is the barrier semantics —
    /// that lives in the kernel — so what this pins is the seam: the call
    /// succeeds on a real file on the filesystems the tests run on, both
    /// with the default (F_BARRIERFSYNC on macOS) and with the strict
    /// opt-in path's primitive (`sync_data`, exercised directly since the
    /// env override is process-wide and cached).
    fn alloc_fmt_probe_name() -> String {
        format!("probe-{}.wal", std::process::id())
    }

    #[test]
    fn wal_sync_completes_and_preserves_bytes() {
        use std::io::Write as _;
        // Per-process path. A fixed name here made the test fail whenever
        // two runs of this binary overlapped: one truncates the file while
        // the other is reading it back, and the reader reports NotFound on
        // a line that looks like a WAL defect rather than a shared path.
        let dir = std::env::temp_dir().join("spg_wal_sync_test");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join(alloc_fmt_probe_name());
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&path)
            .unwrap();
        f.write_all(b"barrier-me").unwrap();
        super::wal_sync(&f).expect("wal_sync must succeed on a plain file");
        f.sync_data().expect("the strict primitive still works too");
        assert_eq!(std::fs::read(&path).unwrap(), b"barrier-me");
        let _ = std::fs::remove_file(&path);
    }
}
