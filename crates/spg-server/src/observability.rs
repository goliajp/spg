// HTTP framing here is one-shot per request — minimal hand-rolled
// parser, no streaming, no chunked encoding. The lints we allow
// here would otherwise nag every byte-cast.
#![allow(
    clippy::cast_lossless,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
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

/// Emit a log line. When `SPG_LOG_FORMAT=json`, encode as
/// `{"level":"...","msg":"...","key":"val",...}\n`; otherwise
/// match the prior `eprintln!("spg-server: <msg> ...")` shape.
#[allow(dead_code)] // wire-up to startup/auth events queued for follow-up commits
pub fn log_event(level: &str, msg: &str, kvs: &[(&str, &str)]) {
    if json_logging_enabled() {
        let mut line = format!("{{\"level\":\"{level}\",\"msg\":\"{}\"", json_escape(msg));
        for (k, v) in kvs {
            line.push_str(&format!(",\"{k}\":\"{}\"", json_escape(v)));
        }
        line.push_str("}\n");
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
    out
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
                DataType::BigInt | DataType::Date | DataType::Timestamp | DataType::Float => 8,
                DataType::Bool => 1,
                DataType::Char(n) => u64::from(n),
                // Average a half-full VARCHAR; the exact value is
                // operator-knowable but not in the catalog.
                DataType::Varchar(n) => u64::from(n).max(1) / 2,
                DataType::Text | DataType::Json => 64,
                DataType::Numeric { .. } | DataType::Interval => 16,
                // f32 per vector dimension.
                DataType::Vector(dim) => u64::from(dim).saturating_mul(4),
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
