// HTTP framing here is one-shot per request — minimal hand-rolled
// parser, no streaming, no chunked encoding. The lints we allow
// here would otherwise nag every byte-cast.
#![allow(
    clippy::cast_lossless,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::doc_markdown,
    clippy::format_push_string,
    clippy::uninlined_format_args
)]

//! v4.13 observability surface: structured logging + `/healthz` +
//! Prometheus `/metrics`.
//!
//! Logging:
//! - `SPG_LOG_FORMAT=json` → every server event goes out as a
//!   single-line JSON object on stderr instead of human-readable
//!   text. Suitable for loki / cloudwatch / datadog ingestion.
//! - default (env unset or any other value) → existing
//!   `eprintln!`-style text. Backwards-compatible.
//!
//! HTTP listener:
//! - Opt-in via `SPG_HTTP_ADDR=host:port`. When set, a tiny
//!   single-threaded HTTP/1.1 listener handles two endpoints:
//!     - `GET /healthz` → 200 "ok"
//!     - `GET /metrics` → 200 with Prometheus exposition-format
//!       counters (`spg_connections_active`, `spg_queries_total`,
//!       `spg_errors_total`, `spg_server_info`).
//! - The HTTP loop is single-threaded — the metrics endpoint is a
//!   handful of atomic loads; even a busy server doesn't need
//!   more.

use std::collections::HashSet;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;

use spg_storage::{DataType, TableSchema};

/// v4.35 per-table metrics: cap on how many tables get
/// `spg_table_rows{...}` + `spg_table_bytes{...}` exported when no
/// explicit allowlist is set. Defaults to 50 to keep Prometheus
/// cardinality bounded for tenants with thousands of tables.
/// Operators raise it explicitly via `SPG_METRICS_TABLE_TOPN`.
const DEFAULT_TABLE_METRIC_TOPN: usize = 50;

/// Atomic counters surfaced via the `/metrics` endpoint. Cheap to
/// update from anywhere — every increment is a single Relaxed
/// atomic operation.
#[derive(Debug, Default)]
pub struct Metrics {
    pub queries_total: AtomicU64,
    pub errors_total: AtomicU64,
    /// v5.2.2: number of cold-tier segments currently registered on
    /// the engine catalog (sum across tables). Updated by the
    /// freezer thread after each successful demotion. Exposed via
    /// `spg_cold_segments_total`.
    pub cold_segments: AtomicU64,
    /// v5.4.1: total `durability_checkpoint` markers the flusher
    /// thread has successfully appended in async-commit mode.
    /// Stays at 0 in sync-commit mode (the flusher isn't spawned).
    /// Exposed via `spg_flusher_iterations_total`.
    pub flusher_iterations: AtomicU64,
    /// v5.4.1: total flusher-thread iterations that failed (WAL
    /// quota exceeded, ENOSPC, mutex poisoned). Pairs with
    /// `flusher_iterations`: a rising errors counter against a
    /// flatline iterations counter is the operator's signal that
    /// the WAL volume needs attention. Exposed via
    /// `spg_flusher_errors_total`.
    pub flusher_errors: AtomicU64,
    /// v5.4.3: WAL byte offset confirmed durable by the most
    /// recent `durability_checkpoint` marker the flusher
    /// emitted. Updated by the flusher after `sync_data` returns
    /// `Ok(())`; stays at 0 in sync-commit mode (the flusher
    /// isn't spawned). Combined with the current WAL file length
    /// at `/metrics` render time, derives `spg_durability_lag_bytes`.
    pub last_durable_wal_offset: AtomicU64,
    /// v5.4.3: wall-clock microseconds-since-epoch the most
    /// recent successful flusher `sync_data` completed. Updated
    /// alongside `last_durable_wal_offset`; 0 means "no flush
    /// has happened in this process lifetime" (either sync mode
    /// or the flusher hasn't ticked yet). Derives
    /// `spg_durability_lag_seconds` at render time.
    pub last_fsync_us: AtomicU64,
    /// v6.6.3 — sum of SQL byte counts seen by the WAL encoder
    /// since boot, before compression is applied. Derives the
    /// `spg_wal_bytes_uncompressed_total` series. Pairs with
    /// `wal_bytes_compressed_out` for the ratio computation.
    pub wal_bytes_uncompressed_in: AtomicU64,
    /// v6.6.3 — sum of bytes written to the WAL since boot, after
    /// compression. Derives `spg_wal_bytes_compressed_total`.
    pub wal_bytes_compressed_out: AtomicU64,
    /// v6.6.3 — sum of cold-tier segment v1 bytes the freezer
    /// produced since boot, BEFORE the v2 envelope. Derives
    /// `spg_segment_bytes_uncompressed_total`.
    pub segment_bytes_uncompressed_in: AtomicU64,
    /// v6.6.3 — sum of bytes actually written to disk for cold-tier
    /// segments since boot. Equals `segment_bytes_uncompressed_in`
    /// when SPG_SEGMENT_COMPRESSION=none. Derives
    /// `spg_segment_bytes_compressed_total`.
    pub segment_bytes_compressed_out: AtomicU64,
    /// v6.7.6 — number of cold-segment files the boot-time
    /// prefetch worker pool successfully read off disk (Linux:
    /// also `posix_fadvise(WILLNEED)`'d to seed the page cache).
    /// Increments by 1 per segment regardless of size. Derives
    /// `spg_cold_prefetch_hits_total`. A boot that loads N
    /// manifest-listed cold segments lands `N` hits; reconnects
    /// and CHECKPOINTs don't touch this counter.
    pub cold_prefetch_hits: AtomicU64,
}

/// JSON-safe escape: replace `"`, `\\`, and control characters per
/// RFC 8259. Used by the logging path so a SQL string with quotes
/// doesn't break the log line.
fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

/// True when the server should emit JSON-formatted log lines.
/// Cheap enough to check per call site — the env var is read once
/// per process anyway (cached at module load).
pub fn json_logging_enabled() -> bool {
    static CHECKED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *CHECKED.get_or_init(|| {
        std::env::var("SPG_LOG_FORMAT").is_ok_and(|v| v.eq_ignore_ascii_case("json"))
    })
}

/// Build the single-line JSON object for one log event (no trailing
/// newline). Split out from `log_event` purely so the escaping +
/// field assembly are unit-testable without capturing stderr; the
/// byte sequence it returns is exactly what `log_event` used to
/// build inline (the caller appends the `\n`).
fn format_json_event(level: &str, msg: &str, kvs: &[(&str, &str)]) -> String {
    let mut line = format!("{{\"level\":\"{level}\",\"msg\":\"{}\"", json_escape(msg));
    for (k, v) in kvs {
        line.push_str(&format!(",\"{k}\":\"{}\"", json_escape(v)));
    }
    line.push('}');
    line
}

/// Emit a log line. When `SPG_LOG_FORMAT=json`, encode as
/// `{"level":"...","msg":"...","key":"val",...}\n`; otherwise
/// match the prior `eprintln!("spg-server: <msg> ...")` shape.
#[allow(dead_code)] // wire-up to startup/auth events queued for follow-up commits
pub fn log_event(level: &str, msg: &str, kvs: &[(&str, &str)]) {
    if json_logging_enabled() {
        let mut line = format_json_event(level, msg, kvs);
        line.push('\n');
        let _ = std::io::stderr().write_all(line.as_bytes());
    } else {
        let mut line = format!("spg-server: {msg}");
        for (k, v) in kvs {
            line.push_str(&format!(" {k}={v}"));
        }
        line.push('\n');
        let _ = std::io::stderr().write_all(line.as_bytes());
    }
}

/// Spawn the `/healthz` + `/metrics` HTTP listener. Returns the
/// bound address (useful for tests that use `127.0.0.1:0` to pick
/// a free port).
pub fn spawn_http(
    addr: &str,
    state: Arc<crate::ServerState>,
) -> std::io::Result<std::net::SocketAddr> {
    let listener = TcpListener::bind(addr)?;
    let local = listener.local_addr()?;
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else {
                continue;
            };
            let s = Arc::clone(&state);
            thread::spawn(move || {
                if let Err(e) = handle_http(stream, &s) {
                    eprintln!("spg-server: /metrics conn error: {e}");
                }
            });
        }
    });
    Ok(local)
}

fn handle_http(mut stream: TcpStream, state: &crate::ServerState) -> std::io::Result<()> {
    // Read up to 4 KiB of request — enough for any reasonable
    // GET. We stop at the blank-line CRLF terminator.
    let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(5)));
    let mut buf = Vec::with_capacity(4096);
    let mut chunk = [0u8; 1024];
    loop {
        let n = stream.read(&mut chunk)?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
        if buf.windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
        if buf.len() > 4 * 1024 {
            return write_response(&mut stream, 414, "URI Too Long", "request header too large");
        }
    }
    let req = std::str::from_utf8(&buf).unwrap_or("");
    let request_line = req.lines().next().unwrap_or("");
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("");
    let path = parts.next().unwrap_or("");
    match (method, path) {
        ("GET", "/healthz") => write_response(&mut stream, 200, "OK", "ok\n"),
        ("GET", "/metrics") => {
            let body = render_metrics(state);
            write_response(&mut stream, 200, "OK", &body)
        }
        ("GET", _) => write_response(&mut stream, 404, "Not Found", "no such path\n"),
        _ => write_response(&mut stream, 405, "Method Not Allowed", "GET only\n"),
    }
}

fn render_metrics(state: &crate::ServerState) -> String {
    let mut out = String::with_capacity(512);
    let version = env!("CARGO_PKG_VERSION");
    out.push_str("# HELP spg_server_info SPG version build info\n");
    out.push_str("# TYPE spg_server_info gauge\n");
    out.push_str(&format!("spg_server_info{{version=\"{version}\"}} 1\n"));
    out.push_str("# HELP spg_connections_active Current live client connections\n");
    out.push_str("# TYPE spg_connections_active gauge\n");
    out.push_str(&format!(
        "spg_connections_active {}\n",
        state.active_connections.load(Ordering::Relaxed)
    ));
    out.push_str("# HELP spg_queries_total Total queries dispatched\n");
    out.push_str("# TYPE spg_queries_total counter\n");
    out.push_str(&format!(
        "spg_queries_total {}\n",
        state.metrics.queries_total.load(Ordering::Relaxed)
    ));
    out.push_str("# HELP spg_errors_total Total query errors\n");
    out.push_str("# TYPE spg_errors_total counter\n");
    out.push_str(&format!(
        "spg_errors_total {}\n",
        state.metrics.errors_total.load(Ordering::Relaxed)
    ));
    render_table_metrics(state, &mut out);
    render_replication_lag(state, &mut out);
    render_hot_tier(state, &mut out);
    render_cold_tier(state, &mut out);
    render_flusher(state, &mut out);
    render_durability_lag(state, &mut out);
    render_compression(state, &mut out);
    out
}

/// v5.4.3 — durability lag metrics. In sync-commit mode both
/// series are reported as 0 (every write is fsynced before the
/// client ack, so the lag is structurally bounded by one fsync
/// latency — sub-millisecond — and not worth a per-write atomic
/// store on the hot path). In async-commit mode the metrics are
/// derived from `last_durable_wal_offset` + `last_fsync_us`,
/// which the flusher thread updates after every successful
/// `sync_data`. Operators can alert on `lag_bytes` growth to
/// detect a stuck flusher even while `flusher_iterations_total`
/// keeps counting (a tick that fails to grab the WAL mutex
/// could spin without making forward progress; not the current
/// behaviour but defended against by the metric).
fn render_durability_lag(state: &crate::ServerState, out: &mut String) {
    let (lag_bytes, lag_seconds) = if crate::synchronous_commit_disabled() {
        compute_durability_lag(state)
    } else {
        (0u64, 0.0f64)
    };
    out.push_str(
        "# HELP spg_durability_lag_bytes WAL bytes written but not yet covered by a durability_checkpoint marker (v5.4.3)\n",
    );
    out.push_str("# TYPE spg_durability_lag_bytes gauge\n");
    out.push_str(&format!("spg_durability_lag_bytes {lag_bytes}\n"));
    out.push_str(
        "# HELP spg_durability_lag_seconds Seconds since the flusher's most recent successful sync_data (v5.4.3)\n",
    );
    out.push_str("# TYPE spg_durability_lag_seconds gauge\n");
    out.push_str(&format!("spg_durability_lag_seconds {lag_seconds:.6}\n"));
}

/// v6.6.3 — compression ratio series.
fn render_compression(state: &crate::ServerState, out: &mut String) {
    out.push_str(
        "# HELP spg_wal_bytes_uncompressed_total Sum of SQL byte counts seen by the WAL encoder since boot (v6.6.3)\n",
    );
    out.push_str("# TYPE spg_wal_bytes_uncompressed_total counter\n");
    out.push_str(&format!(
        "spg_wal_bytes_uncompressed_total {}\n",
        state
            .metrics
            .wal_bytes_uncompressed_in
            .load(Ordering::Relaxed)
    ));
    out.push_str(
        "# HELP spg_wal_bytes_compressed_total Sum of bytes written to the WAL since boot, after compression (v6.6.3)\n",
    );
    out.push_str("# TYPE spg_wal_bytes_compressed_total counter\n");
    out.push_str(&format!(
        "spg_wal_bytes_compressed_total {}\n",
        state
            .metrics
            .wal_bytes_compressed_out
            .load(Ordering::Relaxed)
    ));
    out.push_str(
        "# HELP spg_segment_bytes_uncompressed_total Sum of cold-tier segment v1 bytes the freezer produced (v6.6.3)\n",
    );
    out.push_str("# TYPE spg_segment_bytes_uncompressed_total counter\n");
    out.push_str(&format!(
        "spg_segment_bytes_uncompressed_total {}\n",
        state
            .metrics
            .segment_bytes_uncompressed_in
            .load(Ordering::Relaxed)
    ));
    out.push_str(
        "# HELP spg_segment_bytes_compressed_total Sum of bytes actually written to disk for cold-tier segments (v6.6.3)\n",
    );
    out.push_str("# TYPE spg_segment_bytes_compressed_total counter\n");
    out.push_str(&format!(
        "spg_segment_bytes_compressed_total {}\n",
        state
            .metrics
            .segment_bytes_compressed_out
            .load(Ordering::Relaxed)
    ));
    out.push_str(
        "# HELP spg_cold_prefetch_hits_total Cold-segment files loaded via the boot-time prefetch worker pool (v6.7.6)\n",
    );
    out.push_str("# TYPE spg_cold_prefetch_hits_total counter\n");
    out.push_str(&format!(
        "spg_cold_prefetch_hits_total {}\n",
        state.metrics.cold_prefetch_hits.load(Ordering::Relaxed)
    ));
}

fn compute_durability_lag(state: &crate::ServerState) -> (u64, f64) {
    let durable_offset = state
        .metrics
        .last_durable_wal_offset
        .load(Ordering::Relaxed);
    let current_wal_len = state
        .wal
        .as_ref()
        .and_then(|m| m.lock().ok())
        .and_then(|f| f.metadata().ok())
        .map_or(0, |md| md.len());
    let lag_bytes = current_wal_len.saturating_sub(durable_offset);
    let last_us = state.metrics.last_fsync_us.load(Ordering::Relaxed);
    let lag_seconds = if last_us == 0 {
        0.0
    } else {
        let now_us = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .ok()
            .and_then(|d| u64::try_from(d.as_micros()).ok())
            .unwrap_or(last_us);
        (now_us.saturating_sub(last_us)) as f64 / 1_000_000.0
    };
    (lag_bytes, lag_seconds)
}

/// v5.4.1 — async-commit flusher counters. Always rendered (even
/// in sync-commit mode where both stay at 0) so a Prometheus
/// dashboard tracking these series doesn't need conditional
/// queries; the zeros are themselves the "sync mode confirmed"
/// signal.
fn render_flusher(state: &crate::ServerState, out: &mut String) {
    out.push_str(
        "# HELP spg_flusher_iterations_total Successful durability_checkpoint emissions by the async-commit flusher (v5.4.1)\n",
    );
    out.push_str("# TYPE spg_flusher_iterations_total counter\n");
    out.push_str(&format!(
        "spg_flusher_iterations_total {}\n",
        state.metrics.flusher_iterations.load(Ordering::Relaxed)
    ));
    out.push_str(
        "# HELP spg_flusher_errors_total Flusher iterations that failed to append a durability marker (v5.4.1)\n",
    );
    out.push_str("# TYPE spg_flusher_errors_total counter\n");
    out.push_str(&format!(
        "spg_flusher_errors_total {}\n",
        state.metrics.flusher_errors.load(Ordering::Relaxed)
    ));
}

/// v5.2.2 — cold-tier segment count. Tracked separately from
/// `render_table_metrics` because segments are catalog-global
/// (`Catalog::cold_segments`), not per-table; a cardinality concern
/// doesn't apply.
fn render_cold_tier(state: &crate::ServerState, out: &mut String) {
    out.push_str(
        "# HELP spg_cold_segments_total Cold-tier segments registered on the engine catalog (v5.2.2)\n",
    );
    out.push_str("# TYPE spg_cold_segments_total gauge\n");
    out.push_str(&format!(
        "spg_cold_segments_total {}\n",
        state.metrics.cold_segments.load(Ordering::Relaxed)
    ));
}

/// v5.2.1 — hot-tier byte counters. `spg_hot_tier_bytes_used` is the
/// catalog-wide sum of every table's `hot_bytes()` (the encoded byte
/// size of currently in-RAM rows). `spg_hot_tier_bytes_budget` is the
/// configured cap (`SPG_HOT_TIER_BYTES`, default 4 GiB). v5.2.1 ships
/// these as measurement only — v5.2.2 will use the same `used / budget`
/// comparison to wake the freezer thread.
fn render_hot_tier(state: &crate::ServerState, out: &mut String) {
    let used = match state.engine.read() {
        Ok(engine) => engine.catalog().hot_tier_bytes(),
        Err(_) => return,
    };
    out.push_str("# HELP spg_hot_tier_bytes_used Encoded byte size of hot-tier rows (v5.2.1)\n");
    out.push_str("# TYPE spg_hot_tier_bytes_used gauge\n");
    out.push_str(&format!("spg_hot_tier_bytes_used {used}\n"));
    out.push_str(
        "# HELP spg_hot_tier_bytes_budget Hot-tier byte budget configured via SPG_HOT_TIER_BYTES\n",
    );
    out.push_str("# TYPE spg_hot_tier_bytes_budget gauge\n");
    out.push_str(&format!(
        "spg_hot_tier_bytes_budget {}\n",
        state.hot_tier_byte_budget
    ));
}

/// v4.36 follower-side replication lag. Emits two series:
///
/// - `spg_replication_lag_bytes` — `primary_pos − follower_applied_pos`
///   from the master's most recent status frame. Zero on a primary
///   or a v1-only follower; both series omitted when the follower
///   hasn't received any status frame yet (so Prometheus doesn't
///   reify a misleading 0 lag).
/// - `spg_replication_lag_seconds` — `now − master_wall_time_us`,
///   converted to floating seconds. Same omit-on-no-data rule.
fn render_replication_lag(state: &crate::ServerState, out: &mut String) {
    let primary_pos = state.lag_state.primary_pos.load(Ordering::Acquire);
    let primary_wall = state.lag_state.primary_wall_time_us.load(Ordering::Acquire);
    if primary_wall == 0 {
        // No status frame seen yet — primary or v1 follower. Skip.
        return;
    }
    let applied = state.lag_state.follower_applied_pos.load(Ordering::Acquire);
    let lag_bytes = primary_pos.saturating_sub(applied);
    let now_us = u64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_micros()),
    )
    .unwrap_or(0);
    // wall-time deltas can go briefly negative under NTP slew —
    // saturate at 0 so the metric is monotonically meaningful.
    let lag_us = now_us.saturating_sub(primary_wall);
    // Microseconds-to-seconds cast is precision-safe for any lag
    // smaller than ~285 years — f64 mantissa covers it.
    #[allow(clippy::cast_precision_loss)]
    let lag_seconds = (lag_us as f64) / 1_000_000.0;
    out.push_str("# HELP spg_replication_lag_bytes WAL bytes follower is behind primary (v4.36 status frame)\n");
    out.push_str("# TYPE spg_replication_lag_bytes gauge\n");
    out.push_str(&format!("spg_replication_lag_bytes {lag_bytes}\n"));
    out.push_str(
        "# HELP spg_replication_lag_seconds Wall-clock seconds since primary's last status frame\n",
    );
    out.push_str("# TYPE spg_replication_lag_seconds gauge\n");
    out.push_str(&format!("spg_replication_lag_seconds {lag_seconds}\n"));
}

/// v4.35: per-table `spg_table_rows{table="..."}` +
/// `spg_table_bytes{table="..."}` series. Operators bound
/// cardinality via either:
///
/// - `SPG_METRICS_TABLE_ALLOWLIST=t1,t2,...` — exact list, in
///   exposition order; tables not listed (or not present) are
///   silently dropped.
/// - `SPG_METRICS_TABLE_TOPN=N` — without an allowlist, only the
///   N largest tables (by row count) are exported (default 50 —
///   the `DEFAULT_TABLE_METRIC_TOPN` constant).
///
/// Reads the engine catalog under `engine.read()`. Cost is one
/// pass over `Catalog::table_names()` + per-table `row_count()` +
/// schema width — none of which allocate row data.
fn render_table_metrics(state: &crate::ServerState, out: &mut String) {
    let Ok(engine) = state.engine.read() else {
        // Engine lock poisoned — leave the per-table series off so
        // /metrics still serves the rest of the page. Operators see
        // this via spg_errors_total.
        return;
    };
    let catalog = engine.catalog();
    let allowlist: Option<HashSet<String>> = std::env::var("SPG_METRICS_TABLE_ALLOWLIST")
        .ok()
        .filter(|s| !s.is_empty())
        .map(|s| {
            s.split(',')
                .map(|t| t.trim().to_string())
                .filter(|t| !t.is_empty())
                .collect()
        });
    let topn = std::env::var("SPG_METRICS_TABLE_TOPN")
        .ok()
        .and_then(|s| s.trim().parse::<usize>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(DEFAULT_TABLE_METRIC_TOPN);

    let mut entries: Vec<(String, u64, u64)> = catalog
        .table_names()
        .into_iter()
        .filter_map(|name| {
            let table = catalog.get(&name)?;
            let rows = table.row_count() as u64;
            let bytes = rows.saturating_mul(approx_row_bytes(table.schema()));
            Some((name, rows, bytes))
        })
        .filter(|(name, _, _)| match &allowlist {
            Some(set) => set.contains(name),
            None => true,
        })
        .collect();

    if allowlist.is_none() {
        // Top-N by row count, tiebreak by name for stable output.
        entries.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        entries.truncate(topn);
    } else {
        // Allowlist defines the user-meaningful order; keep
        // catalog order so the output is deterministic.
        entries.sort_by(|a, b| a.0.cmp(&b.0));
    }

    out.push_str("# HELP spg_table_rows Live row count per user table\n");
    out.push_str("# TYPE spg_table_rows gauge\n");
    for (name, rows, _) in &entries {
        out.push_str(&format!(
            "spg_table_rows{{table=\"{}\"}} {rows}\n",
            metric_label_escape(name)
        ));
    }
    out.push_str("# HELP spg_table_bytes Approximate on-disk byte size per user table (rows × schema width)\n");
    out.push_str("# TYPE spg_table_bytes gauge\n");
    for (name, _, bytes) in &entries {
        out.push_str(&format!(
            "spg_table_bytes{{table=\"{}\"}} {bytes}\n",
            metric_label_escape(name)
        ));
    }
}

/// v4.35: average row width estimate from the schema. Used for
/// `spg_table_bytes`. Variable-width types pick a defensible
/// upper-ish bound — operators who care about exact disk usage
/// inspect the snapshot file directly.
fn approx_row_bytes(schema: &TableSchema) -> u64 {
    schema
        .columns
        .iter()
        .map(|c| -> u64 {
            match c.ty {
                DataType::SmallInt => 2,
                DataType::Int => 4,
                DataType::Real => 4,
                DataType::BigInt
                | DataType::Date
                | DataType::Timestamp
                | DataType::Timestamptz
                | DataType::Float => 8,
                DataType::Bool => 1,
                DataType::Char(n) => u64::from(n),
                // Average a half-full VARCHAR; the exact value is
                // operator-knowable but not in the catalog.
                DataType::Varchar(n) => u64::from(n).max(1) / 2,
                DataType::Text | DataType::Json | DataType::Jsonb => 64,
                // v7.10.4 — same rough sizing as Text. Exact value
                // is operator-knowable; this is a snapshot heuristic.
                DataType::Bytes => 64,
                // v7.10.9 — TEXT[] sized like a small array of TEXT
                // cells; rough heuristic (~4 elements × 16 chars).
                DataType::TextArray => 64,
                // v7.11.12 — INT[] / BIGINT[] rough heuristic
                // (~4 elements × element size).
                DataType::IntArray => 16,
                DataType::BigIntArray => 32,
                // v7.12.0 — tsvector averages ~80 lexemes × ~8B
                // each per the v7.12 design risk register R2.
                DataType::TsVector => 640,
                // tsquery rarely persists as a column (usually a
                // literal); rough sizing matches a small AST.
                DataType::TsQuery => 64,
                // v7.17.0 — UUID is fixed 16 bytes (RFC 4122).
                DataType::Uuid => 16,
                // v7.17.0 Phase 3.P0-32 — TIME is fixed i64 (8 bytes).
                DataType::Time => 8,
                // v7.17.0 Phase 3.P0-33 — YEAR is fixed u16 (2 bytes).
                DataType::Year => 2,
                // v7.17.0 Phase 3.P0-34 — TIMETZ is i64 + i32 (12 bytes).
                DataType::TimeTz => 12,
                // v7.17.0 Phase 3.P0-35 — MONEY is fixed i64 (8 bytes).
                DataType::Money => 8,
                // v7.17.0 Phase 3.P0-38 — range cells avg ~24 bytes
                // (flags + two element values).
                DataType::Range(_) => 24,
                // v7.17.0 Phase 3.P0-39 — hstore avg ~64 bytes
                // (k=>v map, varlena).
                DataType::Hstore => 64,
                // v7.17.0 Phase 3.P0-40 — 2D arrays avg ~96 bytes
                // (4×3 matrix × per-element cost).
                DataType::IntArray2D | DataType::BigIntArray2D | DataType::TextArray2D => 96,
                // v7.37.5 β-P4 — INTERVAL[] sized like a small array
                // (~4 elements × 17B body).
                DataType::IntervalArray => 68,
                // v7.37.5 γ — array-of-scalar rough heuristic
                // (~4 elements × per-element body size).
                DataType::BoolArray => 8,
                DataType::SmallIntArray => 12,
                DataType::FloatArray | DataType::TimestampArray | DataType::TimestamptzArray => 36,
                DataType::DateArray => 20,
                DataType::NumericArray => 72,
                DataType::UuidArray => 68,
                DataType::JsonArray | DataType::JsonbArray => 256,
                DataType::BytesArray => 256,
                DataType::VarcharArray | DataType::CharArray => 64,
                // v7.37.5 δ — multirange rough heuristic (~3
                // ranges × 24 B bounds body).
                DataType::Multirange(_) => 72,
                // v7.37.5 ε — geometry. Fixed-width: Point=16,
                // Lseg/Box=32, Line=24, Circle=24. Variable
                // (Path/Polygon) estimated for ~4 points.
                DataType::Point => 16,
                DataType::Lseg | DataType::PgBox => 32,
                DataType::Line | DataType::Circle => 24,
                DataType::Path => 1 + 4 + 16 * 4,
                DataType::Polygon => 4 + 16 * 4,
                // v7.37.5 ζ-A — network / bit / xml / "char" / money[].
                DataType::Inet | DataType::Cidr => 18,
                DataType::Macaddr => 6,
                DataType::Macaddr8 => 8,
                // BIT(n) avg 32 bytes (256 bits); BIT VARYING varlena
                // wider.
                DataType::Bit | DataType::BitVarying => 32,
                DataType::Xml => 128,
                DataType::Char1 => 1,
                DataType::MoneyArray => 32, // ~4 elements × 9 B

                DataType::Numeric { .. } | DataType::Interval => 16,
                // f32 per vector dimension.
                DataType::Vector { dim, .. } => u64::from(dim).saturating_mul(4),
            }
        })
        .sum()
}

/// Escape `\\` and `"` per Prometheus exposition format (control
/// characters not allowed in label values; we don't generate any).
fn metric_label_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            c => out.push(c),
        }
    }
    out
}

fn write_response(
    stream: &mut TcpStream,
    code: u16,
    reason: &str,
    body: &str,
) -> std::io::Result<()> {
    let response = format!(
        "HTTP/1.1 {code} {reason}\r\nContent-Type: text/plain; charset=utf-8\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(response.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::{format_json_event, json_escape};

    #[test]
    fn json_escape_plain_message_passes_through() {
        assert_eq!(json_escape("hello world"), "hello world");
        // Raw multibyte UTF-8 is valid inside a JSON string and must
        // pass through verbatim (not be \u-escaped).
        assert_eq!(json_escape("café — 日本語"), "café — 日本語");
    }

    #[test]
    fn json_escape_handles_quote_backslash_and_whitespace() {
        // The correctness-critical case: a message carrying a double
        // quote, a backslash, and the three whitespace controls must
        // not break the surrounding JSON string.
        let raw = "a\"b\\c\nd\re\tf";
        assert_eq!(json_escape(raw), "a\\\"b\\\\c\\nd\\re\\tf");
    }

    #[test]
    fn json_escape_other_control_chars_use_u_escape() {
        // NUL (0x00) and BEL (0x07) aren't among the named escapes, so
        // they fall through to the \u00xx form.
        assert_eq!(json_escape("\u{0}\u{7}"), "\\u0000\\u0007");
    }

    #[test]
    fn format_json_event_plain_has_expected_fields_and_order() {
        let line = format_json_event("warn", "slow_query", &[("elapsed_us", "123")]);
        // Exact-string equality doubles as a validity check: this is a
        // hand-verified well-formed JSON object. (The crate carries no
        // serde_json dependency, so we assert the bytes directly rather
        // than round-tripping through a parser.)
        assert_eq!(
            line,
            r#"{"level":"warn","msg":"slow_query","elapsed_us":"123"}"#
        );
    }

    #[test]
    fn format_json_event_stays_single_line_with_hostile_value() {
        // A value containing a double quote and a literal newline: the
        // newline must be escaped to `\n` so the record stays on one
        // physical line, and the quote must not terminate the string.
        let line = format_json_event("error", "boom", &[("sql", "SELECT '\"' \n --x")]);
        assert!(
            !line.contains('\n'),
            "log record must be a single physical line, got: {line}"
        );
        assert_eq!(
            line,
            r#"{"level":"error","msg":"boom","sql":"SELECT '\"' \n --x"}"#
        );
    }

    #[test]
    fn format_json_event_severity_label_is_carried_verbatim() {
        // The `level` string maps straight into the `error_severity`
        // analogue field — assert a couple of the levels used by call
        // sites land unmodified.
        for lvl in ["info", "warn", "error"] {
            let line = format_json_event(lvl, "m", &[]);
            assert_eq!(line, format!(r#"{{"level":"{lvl}","msg":"m"}}"#));
        }
    }
}
