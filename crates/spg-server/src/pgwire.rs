// PG-wire is a hand-rolled binary protocol — casts between integer
// widths (i32 lengths, u16 column counts, etc.) and verbose match
// arms are inherent to the format. Allowlist scoped to this file.
#![allow(
    clippy::cast_lossless,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::doc_markdown,
    clippy::similar_names,
    clippy::too_many_lines,
    clippy::uninlined_format_args,
    clippy::unreadable_literal,
    clippy::assigning_clones
)]

//! v4.3 PostgreSQL wire-protocol compatibility shim.
//!
//! Opt-in: set `SPG_PG_ADDR=127.0.0.1:5545` (or any host:port) and
//! the server starts a second TCP listener that talks the simple
//! PostgreSQL v3 wire protocol. Goal is "psql / DBeaver / Metabase
//! can connect, browse tables, run basic queries" against the same
//! Engine instance.
//!
//! This is NOT a full PG implementation — no extended-query / prepared
//! statements, no SSL handshake (clients should use `sslmode=disable`
//! / `sslmode=allow`), no COPY, no NOTIFY, no replication. It maps:
//!
//! - StartupMessage → AuthenticationCleartextPassword → PasswordMessage
//!   → AuthenticationOk + ParameterStatus + ReadyForQuery
//! - Q (Query) → engine.execute / execute_readonly → RowDescription +
//!   DataRow* + CommandComplete + ReadyForQuery
//! - X (Terminate) → close
//!
//! Auth uses the same RBAC user table as the native wire. The
//! cleartext password comes through the connection — fine for
//! docker-compose intra-network deployments (matches our
//! out-of-scope decision to never ship TLS).

use std::cell::RefCell;
use std::collections::VecDeque;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::thread;

use spg_engine::{CancelToken, EngineError, MonotonicNowFn, QueryResult, Role};
use spg_storage::{ColumnSchema, DataType, Row, Value};

use crate::ServerState;
use crate::mysqlwire::ReadWrite;

// v7.37.x (docker-fair SCALARSQ wire-overhead attack) — per-thread
// parse cache for the simple-query streaming path. The wire-probe
// session sends 100 + identical SQLs sequentially; caching the post-
// `prepare_select_streaming` AST saves ~50-80 µs / query on hit
// (parse + clock + ORDER-BY-position + reorder). Bounded LRU.
const PGWIRE_PARSE_CACHE_CAP: usize = 64;

thread_local! {
    static PGWIRE_PARSE_CACHE: RefCell<
        VecDeque<(String, Arc<spg_engine::SelectStatement>)>,
    > = const { RefCell::new(VecDeque::new()) };
}

fn pgwire_parse_cache_get(sql: &str) -> Option<Arc<spg_engine::SelectStatement>> {
    PGWIRE_PARSE_CACHE.with(|c| {
        let c = c.borrow();
        for (k, s) in c.iter() {
            if k == sql {
                return Some(Arc::clone(s));
            }
        }
        None
    })
}

fn pgwire_parse_cache_put(sql: &str, stmt: Arc<spg_engine::SelectStatement>) {
    PGWIRE_PARSE_CACHE.with(|c| {
        let mut c = c.borrow_mut();
        c.retain(|(k, _)| k != sql);
        if c.len() >= PGWIRE_PARSE_CACHE_CAP {
            c.pop_back();
        }
        c.push_front((sql.to_string(), stmt));
    });
}

const PROTOCOL_V3: u32 = 196608; // 3 << 16

/// Spawn the PG-wire listener thread. Returns once the listener is
/// bound (so the parent can log "listening on …"). Each accepted
/// connection runs its own thread that owns its `Conn` state.
pub fn spawn_listener(
    addr: &str,
    state: Arc<ServerState>,
) -> std::io::Result<std::net::SocketAddr> {
    let listener = TcpListener::bind(addr)?;
    let local = listener.local_addr()?;
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else {
                continue;
            };
            let state = Arc::clone(&state);
            thread::spawn(move || {
                if let Err(e) = handle_conn(stream, &state) {
                    eprintln!("spg-server: pg-wire conn error: {e}");
                }
            });
        }
    });
    Ok(local)
}

/// v7.37.43-T4 — split a PG simple-query SQL body into top-level
/// statements at unquoted, unparenthesized `;` separators.
///
/// PG's simple-query protocol explicitly accepts a script with
/// multiple statements separated by `;` (see PG docs §53.2.2 — "Simple
/// Query"). `sqlx::migrate!()` relies on this: every migration file
/// is submitted as one Query message and the server executes each
/// statement in order. Before this change SPG's parser bailed on
/// the second statement with "expected end of input, got Create",
/// which broke every sqlx-migrate user (sentori, mailrs, and every
/// drop-in port via sqlx 0.8).
///
/// The splitter respects PG lexer boundaries — `;` inside any of
/// the constructs below is NOT a separator:
///   * single-quoted strings `'…'` (incl. `''` escape)
///   * double-quoted identifiers `"…"` (incl. `""` escape)
///   * `--` line comments through end-of-line
///   * `/* … */` block comments (PG nests these — we track depth)
///   * dollar-quoted strings `$tag$ … $tag$` (incl. `$$` empty tag,
///     used by `DO $$ … $$` blocks in sentori migrations 0009 / 0021)
///
/// Returns the slice range for each non-empty statement (no trailing
/// `;`, leading/trailing whitespace preserved — the downstream
/// dispatch trims). Whitespace-only segments between adjacent `;;`
/// are dropped so the caller can rely on every returned slice being
/// dispatchable.
pub(crate) fn split_top_level_statements(body: &[u8]) -> Vec<&[u8]> {
    let mut out: Vec<&[u8]> = Vec::new();
    let mut start = 0usize;
    let mut i = 0usize;
    let n = body.len();
    while i < n {
        let b = body[i];
        match b {
            b'\'' => {
                // Single-quoted string. `''` inside = escaped quote.
                i += 1;
                while i < n {
                    if body[i] == b'\'' {
                        if i + 1 < n && body[i + 1] == b'\'' {
                            i += 2;
                            continue;
                        }
                        i += 1;
                        break;
                    }
                    i += 1;
                }
            }
            b'"' => {
                // Double-quoted identifier. `""` inside = escaped.
                i += 1;
                while i < n {
                    if body[i] == b'"' {
                        if i + 1 < n && body[i + 1] == b'"' {
                            i += 2;
                            continue;
                        }
                        i += 1;
                        break;
                    }
                    i += 1;
                }
            }
            b'-' if i + 1 < n && body[i + 1] == b'-' => {
                // Line comment through end of line.
                i += 2;
                while i < n && body[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if i + 1 < n && body[i + 1] == b'*' => {
                // Block comment — PG nests these.
                i += 2;
                let mut depth = 1u32;
                while i < n && depth > 0 {
                    if i + 1 < n && body[i] == b'/' && body[i + 1] == b'*' {
                        depth += 1;
                        i += 2;
                    } else if i + 1 < n && body[i] == b'*' && body[i + 1] == b'/' {
                        depth -= 1;
                        i += 2;
                    } else {
                        i += 1;
                    }
                }
            }
            b'$' => {
                // Dollar-quoted string: `$tag$…$tag$`. Tag is empty
                // or [A-Za-z_][A-Za-z0-9_]*. If not a valid tag,
                // treat `$` as literal byte.
                let tag_start = i + 1;
                let mut j = tag_start;
                let mut valid_tag = true;
                while j < n && body[j] != b'$' {
                    let c = body[j];
                    let ok = if j == tag_start {
                        c.is_ascii_alphabetic() || c == b'_'
                    } else {
                        c.is_ascii_alphanumeric() || c == b'_'
                    };
                    if !ok {
                        valid_tag = false;
                        break;
                    }
                    j += 1;
                }
                if !valid_tag || j >= n {
                    // Not a valid open tag — treat `$` as literal.
                    i += 1;
                    continue;
                }
                // body[tag_start..j] is the tag (possibly empty),
                // body[i..=j] is `$tag$`.
                let close_len = j - i + 1;
                let close_start = i;
                let close_end = j + 1;
                i = j + 1;
                while i + close_len <= n {
                    if body[i..i + close_len] == body[close_start..close_end] {
                        i += close_len;
                        break;
                    }
                    i += 1;
                }
                if i + close_len > n {
                    // unterminated — consume rest.
                    i = n;
                }
            }
            b';' => {
                let slice = &body[start..i];
                if !slice.iter().all(|c| c.is_ascii_whitespace()) {
                    out.push(slice);
                }
                i += 1;
                start = i;
            }
            _ => {
                i += 1;
            }
        }
    }
    // Trailing piece (no closing `;`).
    if start < n {
        let slice = &body[start..n];
        if !slice.iter().all(|c| c.is_ascii_whitespace()) {
            out.push(slice);
        }
    }
    out
}

/// v7.33 (D, autorun) — the PG-wire simple-query ('Q') handler, split
/// out of `handle_conn` (~195 of its ~528 lines). SET / SHOW / COPY
/// intercepts, the canned-probe answers, then execute + WAL/snapshot
/// persist + result emit. Touches only the simple-query connection
/// state (tx_state, settings) — never the extended-protocol prepared/
/// portal maps. Each former `continue` is a normal `Ok(())` return;
/// the caller loops to the next message. Pure extraction.
///
/// v7.37.43-T4 — multi-statement dispatch. When the SQL body contains
/// N > 1 top-level statements (the canonical `sqlx::migrate!()`
/// input shape — every sentori migration file ships as a multi-stmt
/// script), each statement is dispatched in order through the inner
/// handler with exactly one ReadyForQuery + one TCP flush at the end
/// (PG protocol §53.2.2). Mid-script COPY (which would need a
/// multi-frame interaction the captured wbuf can't proxy) is
/// rejected with SQLSTATE `0A000`. The single-stmt path is
/// unchanged — perf-critical PG-wire workloads (probes, pool
/// keepalives, sqlx extended-protocol prepared queries) never enter
/// the multi-stmt branch.
#[allow(clippy::too_many_arguments)]
fn handle_pg_simple_query(
    stream: &mut dyn ReadWrite,
    body: &[u8],
    state: &Arc<ServerState>,
    conn_state: &Arc<crate::ConnState>,
    role: Role,
    tx_state: &mut u8,
    settings: &mut std::collections::HashMap<String, String>,
    wbuf: &mut Vec<u8>,
) -> std::io::Result<()> {
    // Null-terminated SQL string (typically — psql appends \0).
    let sql_bytes = body.strip_suffix(b"\0").unwrap_or(body);
    // v7.37.43-T4 — multi-statement Q dispatch. `sqlx::migrate!()`
    // and `psql -f script.sql` submit migration files as one Q
    // frame with N statements separated by top-level `;`. The
    // single-stmt path below can only handle one statement, so we
    // detect N > 1 here and route through a capture-and-strip-RFQ
    // loop. Statements that the splitter sees only one of (every
    // probe / pool keepalive / SQL string from sqlx extended
    // protocol Parse, plus simple-query psql REPL lines) fall
    // through unchanged.
    {
        let stmts = split_top_level_statements(sql_bytes);
        if stmts.len() > 1 {
            return dispatch_pg_simple_query_multi(
                stream, &stmts, state, conn_state, role, tx_state, settings, wbuf,
            );
        }
    }
    // v7.39 (read01 round 85 follow-up) — abort-firewall at the wire level,
    // placed ahead of EVERY short-circuit (the pure-int `SELECT <n>` fast path,
    // the SHOW / SET / COPY / canned handlers, and the main execute path). In an
    // aborted transaction ('E') PG rejects every statement except a
    // transaction-control command with 25P02; without this guard a short-circuit
    // answered as if nothing were wrong (`SELECT 1`, `SHOW …`, `SET …`). A
    // transaction-control command is let through to the engine firewall, which
    // ends the block (and downgrades a COMMIT to a ROLLBACK).
    if *tx_state == b'E' {
        // Strip a trailing `;` before reading the verb — `COMMIT;` must still be
        // recognised as transaction control, or the block would never end.
        let mut vb = trim_ascii(sql_bytes);
        if vb.last() == Some(&b';') {
            vb = trim_ascii(&vb[..vb.len() - 1]);
        }
        let verb = vb
            .split(|&b| b == b' ' || b == b'\t' || b == b'\n' || b == b'\r')
            .next()
            .unwrap_or(b"");
        let is_tx_control = ci_eq(verb, b"commit")
            || ci_eq(verb, b"rollback")
            || ci_eq(verb, b"end")
            || ci_eq(verb, b"abort");
        if !is_tx_control {
            send_error(
                wbuf,
                "25P02",
                "current transaction is aborted, commands ignored until end of transaction block",
            )?;
            send_ready_for_query(wbuf, *tx_state)?;
            stream.write_all(wbuf)?;
            wbuf.clear();
            return Ok(());
        }
    }
    // v7.37.x (SPGS PLUCK 红线) — ultra-hot early-out for pure-int
    // `SELECT <int>` BEFORE the per-query activity-registry update
    // (RWLock::write + String alloc per query). Liveness probes /
    // pool keepalives don't need to show up in `spg_stat_activity` —
    // an integer ping has no diagnostic value to surface there.
    // Saves ~2-3 µs / query at the floor.
    {
        let trimmed_bytes = trim_ascii(sql_bytes);
        let trimmed_bytes = if trimmed_bytes.last() == Some(&b';') {
            trim_ascii(&trimmed_bytes[..trimmed_bytes.len() - 1])
        } else {
            trimmed_bytes
        };
        if let Some(rest) = ci_strip_prefix(trimmed_bytes, b"select ") {
            let rest_trim = trim_ascii(rest);
            if !rest_trim.is_empty()
                && rest_trim
                    .iter()
                    .all(|c| c.is_ascii_digit() || *c == b'-' || *c == b'+')
                && let Ok(s) = core::str::from_utf8(rest_trim)
                && let Ok(n) = s.parse::<i64>()
            {
                // v7.39 (round 222) — flush pending LISTEN/NOTIFY deliveries
                // even on the liveness fast path: psycopg2 / libpq poll
                // loops issue exactly `SELECT 1` expecting queued
                // notifications to arrive with it. One uncontended Mutex
                // lock (normally-empty Vec) — no engine lock touched.
                let mine: Vec<(String, String)> = conn_state
                    .notify_queue
                    .lock()
                    .map(|mut q| core::mem::take(&mut *q))
                    .unwrap_or_default();
                for (channel, payload) in &mine {
                    let mut body = Vec::with_capacity(channel.len() + payload.len() + 8);
                    body.extend_from_slice(&conn_state.pid.to_be_bytes());
                    body.extend_from_slice(channel.as_bytes());
                    body.push(0);
                    body.extend_from_slice(payload.as_bytes());
                    body.push(0);
                    send_msg(wbuf, b'A', &body)?;
                }
                encode_select_int_response(wbuf, n, *tx_state)?;
                stream.write_all(wbuf)?;
                wbuf.clear();
                return Ok(());
            }
        }
    }
    // v6.5.2 — update activity registry.
    // v7.37.42-arena Phase 5 fallback — derive wall-clock from boot
    // (Instant, SystemTime) pair + monotonic delta instead of paying
    // the per-query `SystemTime::now()` syscall. `current_sql` write
    // also reuses the existing String buffer (clear + push_str) to
    // skip the per-query heap alloc when the SQL is valid UTF-8 —
    // the common case for psql-shaped clients.
    // v7.39 (round 279) — announce whose session this is before the
    // engine touches any session state. One shared Engine serves every
    // connection, so without this the prepared statements, the
    // string-literal dialect and the GUC overrides of one client were
    // visible to all of them.
    if let Ok(mut e) = state.engine.write() {
        e.set_current_session(conn_state.pid);
    }
    let now_us = wallclock_unix_micros();
    conn_state
        .last_query_start_us
        .store(now_us, std::sync::atomic::Ordering::Relaxed);
    if let Ok(mut s) = conn_state.current_sql.write() {
        s.clear();
        match std::str::from_utf8(sql_bytes) {
            Ok(valid) => s.push_str(valid),
            Err(_) => s.push_str(&String::from_utf8_lossy(sql_bytes)),
        }
    }
    let Ok(sql_str) = std::str::from_utf8(sql_bytes) else {
        send_error(wbuf, "22021", "invalid UTF-8 in query")?;
        send_ready_for_query(wbuf, *tx_state)?;
        stream.write_all(wbuf)?;
        wbuf.clear();
        return Ok(());
    };
    // v7.34.3 — keep the trimmed SQL as a &str slice into the
    // incoming buffer. The previous `to_string()` paid a heap
    // allocation on EVERY query just so the downstream `&sql`
    // borrows would outlive the let. Downstream consumers
    // (`parse_set/show/copy_intent`, `canned_response`,
    // `execute_with_role`, `persist_wire_write`) all take `&str`,
    // so the slice form is drop-in.
    let sql: &str = sql_str.trim_end_matches(';').trim();
    // v4.17: COPY ... FROM STDIN / TO STDOUT runs its own
    // multi-frame protocol; intercept before the regular
    // execute path tries to parse them.
    // v4.19: SET name=value / SET name TO value /
    // SET SESSION name=value / SET LOCAL name=value.
    // We store the assignment and return CC "SET";
    // SPG doesn't act on the value (most are
    // client-side hints), but `SHOW name` later
    // returns what was stored.
    // v7.14.0 — SET dispatch is two-tiered:
    //   1. Engine-affecting (FOREIGN_KEY_CHECKS,
    //      session_replication_role, default_text_search_config)
    //      and multi-assignment SET (mysqldump preamble
    //      `SET @OLD_FK_CHECKS=@@FK_CHECKS, FK_CHECKS=0`)
    //      fall through to the engine so the flag
    //      flips.
    //   2. All other SETs (search_path / client_encoding /
    //      timezone / …) stay intercepted to keep the
    //      engine RWLock contention low.
    if let Some((name, value)) = parse_set_statement(sql) {
        let name_lc = name.to_ascii_lowercase();
        settings.insert(name_lc.clone(), value.clone());
        // v7.17 Phase 2.4 — `application_name` is a
        // session GUC; mirror it onto ConnState so
        // `spg_stat_activity.application_name` reflects
        // the live value. PG's own `application_name`
        // is session-scoped — `SET LOCAL` does NOT scope
        // it to a tx (its GUC context is U / S, not L),
        // so we don't special-case SET LOCAL here either.
        if name_lc == "application_name" {
            if let Ok(mut g) = conn_state.application_name.write() {
                *g = value.clone();
            }
        }
        // v7.39 (GUC) — dual-write: the wire cache serves the hot
        // statement_timeout lookup, and the statement ALWAYS falls
        // through to the engine so session_params (the store
        // current_setting()/engine features read) stays in sync.
        // SET is rare; the extra engine write-lock is noise.
        let _ = &value;
    }
    // v7.39 (GUC) — RESET name / RESET ALL: drop the wire-cache
    // entry so SHOW stops serving the stale override, then fall
    // through — the engine's ResetParameter clears session_params.
    {
        let t = sql.trim();
        let b = t.as_bytes();
        let restore_app_name = |settings: &mut std::collections::HashMap<String, String>| {
            if !conn_state.startup_app_name.is_empty() {
                settings.insert(
                    "application_name".to_string(),
                    conn_state.startup_app_name.clone(),
                );
            }
            if let Ok(mut g) = conn_state.application_name.write() {
                g.clone_from(&conn_state.startup_app_name);
            }
        };
        // v7.39 (round 320, V53) — DISCARD ALL includes RESET ALL, so it
        // has to drop the wire cache too; otherwise SHOW kept serving an
        // override the engine had already thrown away.
        if ci_eq(b, b"reset all") || ci_starts_with(b, b"discard all") {
            settings.clear();
            restore_app_name(settings);
        } else if ci_starts_with(b, b"reset ") {
            let name = t[6..]
                .trim()
                .trim_end_matches(';')
                .trim()
                .to_ascii_lowercase();
            settings.remove(&name);
            if name == "application_name" {
                restore_app_name(settings);
            }
        }
    }
    // v4.19: SHOW name / SHOW ALL.
    if let Some(name) = parse_show_statement(sql) {
        // v7.39 (GUC) — the engine store wins: it sees SET LOCAL
        // (tx-scoped undo) and engine-side writes the wire cache
        // can't. Fall back to the cache + known defaults.
        let engine_val: Option<String> = if name != "all" {
            state
                .engine
                .read()
                .ok()
                .and_then(|e| e.session_param(&name).map(str::to_string))
        } else {
            None
        };
        let resp = match engine_val {
            Some(v) => CannedResponse::Rows {
                columns: vec![ColumnSchema::new(name.clone(), DataType::Text, false)],
                rows: vec![Row::new(vec![Value::text(v)])],
            },
            None => render_show(&name, settings),
        };
        send_canned(wbuf, &resp)?;
        send_ready_for_query(wbuf, *tx_state)?;
        stream.write_all(wbuf)?;
        wbuf.clear();
        return Ok(());
    }
    // v7.39 (round 343, V40) — `lo_import` / `lo_export` are the only
    // lo_* calls that touch a server file, and the engine is `no_std`.
    // Same contract COPY-from-a-file uses since round 249: the engine
    // owns the shape and every message, the host owns the `std::fs`.
    if let Some(call) = spg_engine::largeobject::parse_lo_file_call(sql) {
        handle_lo_file_call(wbuf, state, role, &call)?;
        send_ready_for_query(wbuf, *tx_state)?;
        stream.write_all(wbuf)?;
        wbuf.clear();
        return Ok(());
    }
    if let Some(copy) = parse_copy_intent(sql) {
        // COPY mode handles its own protocol roundtrips —
        // flush any pending output so the COPY handler
        // starts from a clean wire state.
        if !wbuf.is_empty() {
            stream.write_all(wbuf)?;
            wbuf.clear();
        }
        match copy {
            CopyIntent::From(table, cols, opts) => {
                handle_copy_from_stdin(
                    stream,
                    state,
                    role,
                    &table,
                    cols.as_deref(),
                    &opts,
                    tx_state,
                    conn_state.tx_id,
                )?;
            }
            CopyIntent::FromFile(spec) => {
                handle_copy_from_file(stream, state, role, &spec, tx_state, conn_state.tx_id)?;
            }
            CopyIntent::ToFile(spec) => {
                handle_copy_to_file(stream, state, role, &spec)?;
            }
            CopyIntent::BadOption(name) => {
                send_error(
                    stream,
                    "42601",
                    &format!("option \"{name}\" not recognized"),
                )?;
                send_ready_for_query(stream, *tx_state)?;
            }
            CopyIntent::To(table, opts) => {
                let sql = format!("SELECT * FROM {table}");
                handle_copy_to_stdout(
                    stream,
                    state,
                    role,
                    &sql,
                    &opts,
                    tx_state,
                    conn_state.tx_id,
                )?;
            }
            CopyIntent::ToQuery(query, opts) => {
                handle_copy_to_stdout(
                    stream,
                    state,
                    role,
                    &query,
                    &opts,
                    tx_state,
                    conn_state.tx_id,
                )?;
            }
        }
        send_ready_for_query(wbuf, *tx_state)?;
        stream.write_all(wbuf)?;
        wbuf.clear();
        return Ok(());
    }
    // psql sends startup probes like "SELECT version()" /
    // "SHOW search_path". Stub the common ones with sane
    // canned answers so the client doesn't error out.
    if let Some(canned) = canned_response(sql, state) {
        send_canned(wbuf, &canned)?;
        send_ready_for_query(wbuf, *tx_state)?;
        stream.write_all(wbuf)?;
        wbuf.clear();
        return Ok(());
    }
    // v6.5.5 — wait_event = write_lock around the
    // engine lock acquisition. Cleared in all paths
    // below (success, error, panic via guard would be
    // overkill — execute_with_role returns Result).
    conn_state
        .wait_event
        .store(1, std::sync::atomic::Ordering::Relaxed);
    // v7.17.0 Phase 2.3 — resolve per-statement deadline
    // from the session `statement_timeout` GUC (default
    // `0` → `CancelToken::none()`, hot path unchanged).
    // v7.39 (query cancel) — arm this statement with the session's
    // CancelRequest flag (cleared first: an idle-time cancel must not
    // kill the NEXT statement, matching PG).
    conn_state
        .cancel_flag
        .store(false, std::sync::atomic::Ordering::Relaxed);
    let cancel = statement_cancel(settings, &conn_state.cancel_flag);
    // v7.37.x (SPGS PROJ wire encode tax) — streaming simple-query
    // SELECT. When the SQL is a pure read (first word == SELECT, no
    // sequence-mutating function), bypass the materialising
    // `execute_with_role → QueryResult::Rows { rows: Vec<Row> }` path
    // and stream rows from the engine straight into `wbuf`. Saves the
    // engine-side `Vec<Row>` allocation (25 k Row alloc + 125 k cell
    // clone on the wire-probe PROJ shape) + the read-then-iterate
    // tax in the response loop below. Non-streamable shapes
    // (aggregate, ORDER BY, DISTINCT, subqueries, …) bail out of the
    // streaming engine pass and the caller's `Vec<Row>` path still
    // runs them.
    let streaming_select_attempt = {
        let trimmed_start = sql.trim_start();
        let first = trimmed_start.split_ascii_whitespace().next().unwrap_or("");
        // SELECT only; CTE / SHOW / EXPLAIN keep the materialising
        // path. setval / nextval / currval mutate sequence state and
        // must hit the write lock, not the read-streaming path.
        if first.eq_ignore_ascii_case("select") {
            // v7.37.42-arena Phase 5 fallback — single-pass scan for
            // sequence-mutating tokens. Replaces 3 × ci_contains
            // independent passes (~2-4 µs saved on 100-byte SQL).
            let b = sql.as_bytes();
            // v7.39 (round 295, E3 Phase 1b) — a SELECT asking for row
            // locks must NOT stream: this path reads under the shared
            // read lock and never reaches the write dispatch, so the
            // locking pre-pass could not run and `FOR UPDATE` was
            // silently ignored in autocommit — where a queue worker
            // runs it. Three layers routed around the lock table
            // (streaming, `is_read`, dispatch ordering); this is the
            // outermost.
            let wants_locks = ci_contains(b, b" for update")
                || ci_contains(b, b" for share")
                || ci_contains(b, b" for no key update")
                || ci_contains(b, b" for key share");
            if !sql_has_sequence_mutator(b) && !wants_locks {
                Some(())
            } else {
                None
            }
        } else {
            None
        }
    };
    // v7.39 (read01 round 84) — the streaming SELECT fast path reads through a
    // read lock against the COMMITTED base tables; it does not thread an open
    // transaction's uncommitted working set. So inside an explicit `BEGIN … `
    // block a SELECT streamed here could not see the transaction's own prior
    // INSERT / UPDATE / DELETE — `BEGIN; INSERT …; SELECT count(*)` returned 0
    // over the wire (read-your-own-writes broken), and a follow-up write then
    // tripped a duplicate-key error against the row it "could not see". The
    // embedded API never hit this because it always uses the materialising
    // `execute_with_role` path. When THIS connection is in a transaction, skip
    // streaming and fall to that path, which respects the transaction snapshot.
    //
    // The witness is the PER-CONNECTION tx_state, not the engine's global
    // in_transaction(): the engine is shared across connections, so a global
    // check would misfire — an autocommit read on one connection would be
    // dragged onto the write path (and see uncommitted data) merely because a
    // DIFFERENT connection had a transaction open.
    // v7.39 (read01 round 85 follow-up) — 'T' is an open transaction, 'E' is an
    // ABORTED one (a statement errored inside it). Both must route reads to the
    // firewalled write path: 'T' so a SELECT sees the transaction's own writes
    // (round 84), 'E' so a SELECT / SHOW in an aborted block is rejected with
    // 25P02 instead of silently running against committed data. The read-only
    // `&self` path checks neither.
    let conn_in_tx = matches!(*tx_state, b'T' | b'E');
    if streaming_select_attempt.is_some() && !conn_in_tx {
        let engine_lock = state
            .engine
            .read()
            .map_err(|_| std::io::Error::other("engine rwlock poisoned"))?;
        let pre_len = wbuf.len();
        // v7.39 (GUC knife 3) — snapshot the session render style once
        // per statement for the row encoders.
        let wire_style = engine_lock.render_style();
        let wire_tz = engine_lock.session_tz();
        let mut cols_storage: Vec<ColumnSchema> = Vec::new();
        let mut wrote_header = false;
        let mut first_row_size: Option<usize> = None;
        // v7.37.42-arena Phase 3 — per-SELECT bumpalo arena hosts the
        // wire-encode fallback-cell text payloads. The Phase 2 arena
        // for SCALARSQ-shape engine-side row scratch lives inside the
        // `take_scalarsq_streaming` branch (it's tied to the engine
        // executor's lifetime — different scope than the wire arena).
        // This outer arena drops at end of the streaming branch in
        // O(1), releasing all per-cell text strings from the
        // value-to-text fallback path (Float / Numeric / Vector /
        // Uuid / range / hstore / array etc.) in a single bulk reset.
        let wire_arena = bumpalo::Bump::new();
        // v7.37.x (docker-fair SCALARSQ wire-overhead attack) — try
        // the per-connection parse cache. On hit we skip the SQL-
        // string entry point's parse / clock / reorder work and call
        // the prepared variant via an Arc-shared AST (no clone). For
        // SQLs containing clock-rewrite-eligible nodes
        // (`current_timestamp` / `now` / `clock_timestamp`) we
        // bypass the cache so the clock value can't go stale —
        // wire-probe SCALARSQ SQLs don't hit this gate.
        let sql_b = sql.as_bytes();
        // v7.37.42-arena Phase 5 fallback — single-pass cache-
        // eligibility scan. The original 6 × ci_contains scans were
        // ~6 × O(n × m) over the SQL bytes (one full pass per needle)
        // and added measurable wire overhead on every probe; the
        // combined scan walks `sql_b` once and short-circuits on the
        // first clock-function hit. Net cumulative win ~3-8 µs on
        // 100-byte SCALARSQ SQL; sub-noise per call, paid back at the
        // T1-fallback cumulative-attack level.
        let cache_eligible = !sql_has_clock_function(sql_b);
        let cached_stmt = if cache_eligible {
            pgwire_parse_cache_get(sql)
        } else {
            None
        };
        let prepared_stmt = if let Some(s) = cached_stmt {
            Some(s)
        } else if let Ok(s) = engine_lock.prepare_select_streaming(sql) {
            let arc = Arc::new(s);
            if cache_eligible {
                pgwire_parse_cache_put(sql, Arc::clone(&arc));
            }
            Some(arc)
        } else {
            None
        };
        // Factor the emit closure body once so cache-hit and miss
        // paths share the encode logic without duplication.
        let mut emit = |item: spg_engine::StreamItem<'_>| -> Result<(), spg_engine::EngineError> {
            match item {
                spg_engine::StreamItem::Header(cols) => {
                    cols_storage.extend_from_slice(cols);
                    let r = send_row_description(wbuf, cols);
                    if r.is_ok() {
                        wrote_header = true;
                    }
                    r.map_err(|e| spg_engine::EngineError::Unsupported(e.to_string()))
                }
                spg_engine::StreamItem::Row(values) => {
                    if first_row_size.is_none() {
                        let before = wbuf.len();
                        let r = encode_data_row_from_refs(
                            wbuf,
                            &cols_storage,
                            values,
                            &wire_arena,
                            &wire_style,
                            &wire_tz,
                        );
                        first_row_size = Some(wbuf.len() - before);
                        return r.map_err(|e| spg_engine::EngineError::Unsupported(e.to_string()));
                    }
                    encode_data_row_from_refs(
                        wbuf,
                        &cols_storage,
                        values,
                        &wire_arena,
                        &wire_style,
                        &wire_tz,
                    )
                    .map_err(|e| spg_engine::EngineError::Unsupported(e.to_string()))
                }
            }
        };
        // v7.37.x (docker-fair SCALARSQ wire-overhead attack) — for
        // shapes the engine materialises internally anyway (anything
        // with a subquery — including the SCALARSQ `(SELECT … FROM
        // …)`-in-projection shape — and all aggregates), the streaming
        // wrapper's emit closure + cell_refs Vec management adds
        // ~25-50 µs of pure overhead for zero benefit. Take the
        // materialised path directly: call
        // `execute_readonly_select_prepared`, then iterate the
        // resulting `Vec<Row>` straight into `encode_data_row`. The
        // streaming wrapper still runs for prepared SELECTs without
        // subqueries (joined non-aggregate projection — the original
        // PROJ-shape consumer).
        // v7.37.x (docker-fair SCALARSQ wire-overhead attack) —
        // shapes the engine materialises internally anyway (anything
        // with a subquery — including the SCALARSQ shape — and all
        // aggregates) take the direct materialised path: call the
        // prepared SELECT and iterate `Vec<Row>` straight into
        // `encode_data_row`, skipping the streaming wrapper's emit
        // closure + cell_refs Vec management. The streaming wrapper
        // still runs for prepared SELECTs without subqueries (joined
        // non-aggregate projection — the original PROJ consumer).
        // v7.37.42-arena Phase 2 — SCALARSQ streaming-shape dispatch.
        // For the narrow SCALARSQ shape (single-table SELECT with at
        // least one scalar subquery in projection + no ORDER BY /
        // GROUP BY / DISTINCT / window / UNION / CTE — see
        // `spg_engine::scalarsq_streaming::is_scalarsq_streaming_shape`),
        // call the slim arena-aware streaming executor that emits
        // each projected row straight into `wbuf` via the encode
        // closure below. Skips the engine-side `Vec<Row>`
        // materialise the `execute_readonly_select_prepared` path
        // pays for shape-forced materialisation; the per-row
        // projection scratch lives in a bumpalo arena that bulk-
        // resets at end of `Bump::new` scope. The wire-side encoded
        // bytes accumulate in `wbuf` so the final write_all is still
        // a single TCP syscall.
        let take_scalarsq_streaming = prepared_stmt.as_ref().is_some_and(|s| {
            spg_engine::scalarsq_streaming::is_scalarsq_streaming_shape(s.as_ref())
        });
        // v7.37.43 (DISTA A-1) — the streaming wrapper's emit closure
        // + per-row `cell_refs` Vec rebuild add ~25 µs of pure overhead
        // for aggregate-bearing shapes whose engine path
        // (`try_exec_joined_streaming`) rejects on aggregate and falls
        // back to a materialised `Vec<Row>` anyway. Take the
        // materialised path directly for aggregates too — same wire
        // bytes, no closure indirection.
        let take_materialised_path = !take_scalarsq_streaming
            && prepared_stmt.as_ref().is_some_and(|s| {
                spg_engine::expr_tree_has_subquery(s.as_ref())
                    || spg_engine::aggregate::uses_aggregate(s.as_ref())
            });
        let stream_result: Result<usize, spg_engine::EngineError> = if take_scalarsq_streaming {
            let s = prepared_stmt
                .as_ref()
                .expect("guarded by is_some_and above");
            // Per-query bumpalo arena — drops in O(1) at scope end,
            // releasing the per-row projection scratch + (Phase 3)
            // arena-allocated wire-cell payloads in one bulk reset.
            // Models PG's per-query MessageContext + printtup.
            let arena = bumpalo::Bump::new();
            (|| -> Result<usize, spg_engine::EngineError> {
                // RowDescription is deferred until first row callback
                // (executor returns columns alongside the emit) so we
                // emit it on the first row callback. An empty result
                // still owes the RowDescription, sent below the loop.
                let mut header_written = false;
                let emit_row = |columns: &[spg_storage::ColumnSchema],
                                values: &[spg_storage::Value<'_>]|
                 -> Result<(), spg_engine::EngineError> {
                    if !header_written {
                        cols_storage.extend_from_slice(columns);
                        send_row_description(wbuf, columns)
                            .map_err(|e| spg_engine::EngineError::Unsupported(e.to_string()))?;
                        header_written = true;
                    }
                    let before = wbuf.len();
                    encode_data_row_from_values(
                        wbuf,
                        &cols_storage,
                        values,
                        &arena,
                        &wire_style,
                        &wire_tz,
                    )
                    .map_err(|e| spg_engine::EngineError::Unsupported(e.to_string()))?;
                    if first_row_size.is_none() {
                        first_row_size = Some(wbuf.len() - before);
                    }
                    Ok(())
                };
                let (columns, n) = engine_lock.execute_readonly_select_with_arena(
                    s.as_ref(),
                    cancel,
                    &arena,
                    emit_row,
                )?;
                if !header_written {
                    cols_storage.extend_from_slice(&columns);
                    send_row_description(wbuf, &columns)
                        .map_err(|e| spg_engine::EngineError::Unsupported(e.to_string()))?;
                }
                wrote_header = true;
                Ok(n)
            })()
        } else if take_materialised_path {
            let s = prepared_stmt
                .as_ref()
                .expect("guarded by is_some_and above");
            (|| -> Result<usize, spg_engine::EngineError> {
                match engine_lock.execute_readonly_select_prepared(s.as_ref(), cancel)? {
                    spg_engine::QueryResult::Rows { columns, rows } => {
                        cols_storage.extend_from_slice(&columns);
                        send_row_description(wbuf, &columns)
                            .map_err(|e| spg_engine::EngineError::Unsupported(e.to_string()))?;
                        wrote_header = true;
                        for row in &rows {
                            let before = wbuf.len();
                            encode_data_row(
                                wbuf,
                                &cols_storage,
                                row,
                                &wire_arena,
                                &wire_style,
                                &wire_tz,
                            )
                            .map_err(|e| spg_engine::EngineError::Unsupported(e.to_string()))?;
                            if first_row_size.is_none() {
                                first_row_size = Some(wbuf.len() - before);
                            }
                        }
                        Ok(rows.len())
                    }
                    _ => Err(spg_engine::EngineError::Unsupported(
                        "select returned non-Rows".into(),
                    )),
                }
            })()
        } else if let Some(s) = prepared_stmt.as_ref() {
            engine_lock.execute_readonly_select_streaming_prepared(s.as_ref(), cancel, &mut emit)
        } else {
            engine_lock.execute_readonly_select_streaming(sql, cancel, &mut emit)
        };
        drop(prepared_stmt);
        drop(engine_lock);
        conn_state
            .wait_event
            .store(0, std::sync::atomic::Ordering::Relaxed);
        match stream_result {
            Ok(n) => {
                // v7.39 (round 318, V51) — and the statement's diagnostics.
                // The streaming fast path used to skip `drain_notices`
                // entirely, so a NOTICE or WARNING raised by a SELECT was
                // dropped on the floor. Nothing raised one until
                // pg_cancel_backend / pg_terminate_backend started
                // reporting an unknown pid, which is how the hole showed.
                // The rows are already in `wbuf`, so it lands just before
                // CommandComplete rather than ahead of the rows.
                drain_notices(state, wbuf)?;
                // v7.39 (round 222) — the streaming fast path also flushes
                // pending LISTEN/NOTIFY deliveries (the engine read lock is
                // dropped above, so the drain's write lock is safe).
                drain_notifications(state, wbuf, conn_state)?;
                send_command_complete_select_count(wbuf, n)?;
                send_ready_for_query(wbuf, *tx_state)?;
                stream.write_all(wbuf)?;
                wbuf.clear();
                return Ok(());
            }
            Err(_e) if !wrote_header => {
                // Streaming refused this SELECT (non-streamable shape
                // or pre-exec parse error). Rewind wbuf and fall
                // through to the materialising path so the error
                // surfaces from the canonical handler.
                wbuf.truncate(pre_len);
            }
            Err(e) => {
                // Error mid-stream after partial output — too late to
                // recover; truncate the partial frames and surface as
                // a wire error so the client doesn't see a torn row.
                wbuf.truncate(pre_len);
                let (sqlstate, msg) = engine_error_to_wire_conn(&e, conn_state);
                send_error_pos(wbuf, sqlstate, &msg, parse_error_position(&e, sql))?;
                send_ready_for_query(wbuf, *tx_state)?;
                stream.write_all(wbuf)?;
                wbuf.clear();
                return Ok(());
            }
        }
        // Restart the wait_event flag for the fall-back path below.
        conn_state
            .wait_event
            .store(1, std::sync::atomic::Ordering::Relaxed);
    }
    // v7.39 (read01 round 85 follow-up) — a COMMIT issued in an aborted
    // transaction is downgraded to a ROLLBACK by the engine firewall; PG tags
    // the wire response ROLLBACK, not COMMIT. Capture the pre-execute abort
    // state so the tag below can reflect the downgrade.
    let was_aborted = *tx_state == b'E';
    // r178 — plain autocommit DML goes through the commit barrier
    // (group fsync, already persisted by the leader); everything else
    // keeps the inline execute + persist_wire_write flow.
    let (result, queue_persisted) =
        match try_queue_plain_dml(state, sql, role, *tx_state, settings) {
            Some(r) => (r, true),
            None => (
                execute_with_role(
                    state,
                    sql,
                    role,
                    cancel,
                    matches!(*tx_state, b'T' | b'E'),
                    conn_state.tx_id,
                    settings,
                ),
                false,
            ),
        };
    conn_state
        .wait_event
        .store(0, std::sync::atomic::Ordering::Relaxed);
    drain_notices(state, wbuf)?;
    drain_notifications(state, wbuf, conn_state)?;
    // v7.33 (A1) — persist the write (WAL/snapshot + audit)
    // before acking it; a durability failure surfaces as a
    // query error, never a false CommandComplete.
    let result = if queue_persisted {
        result
    } else {
        match persist_wire_write(state, sql, &result, conn_state.tx_id) {
            Ok(()) => result,
            Err(e) => Err(EngineError::Unsupported(format!(
                "durability append failed: {e}"
            ))),
        }
    };
    match result {
        Ok(QueryResult::Rows { columns, rows }) => {
            send_row_description(wbuf, &columns)?;
            let n = rows.len();
            // v7.37.x (SPGS PROJ wire encode tax) — calibrate the
            // per-row reservation against the first encoded row so a
            // 25 k-row PROJ doesn't trigger a mid-write `Vec::reserve`
            // (each grow re-memcpys the accumulated buffer — at 25 k
            // rows × ~100 B/row = 2.5 MB the doubling sequence from
            // 1.6 MB reserved costs ~1.6 MB extra memcpy). Encode one
            // row first to learn its byte size, then `reserve` enough
            // headroom for the remaining (n - 1) rows + the
            // CommandComplete tail.
            // v7.37.42-arena Phase 3 — fallback materialised path
            // also gets a per-SELECT arena for the cell text-format
            // fallback payloads. Dropped at end of branch.
            let mat_arena = bumpalo::Bump::new();
            let (wire_style, wire_tz) = state
                .engine
                .read()
                .map(|e| (e.render_style(), e.session_tz()))
                .unwrap_or((Default::default(), spg_engine::SessionTz::Utc));
            if let Some((first, rest)) = rows.split_first() {
                let before = wbuf.len();
                encode_data_row(wbuf, &columns, first, &mat_arena, &wire_style, &wire_tz)?;
                let first_size = wbuf.len() - before;
                if rest.len() > 0 {
                    wbuf.reserve(first_size.saturating_mul(rest.len()).saturating_add(32));
                }
                for row in rest {
                    encode_data_row(wbuf, &columns, row, &mat_arena, &wire_style, &wire_tz)?;
                }
            }
            // v7.39 (round 131) — a RETURNING result keeps its DML tag
            // (`INSERT 0 n` / `UPDATE n` / `DELETE n` / `MERGE n`), only a real
            // SELECT/VALUES/SHOW tags `SELECT n`.
            send_command_complete(wbuf, &command_tag_for_rows_sql(sql, n))?;
        }
        Ok(QueryResult::CommandOk { affected, .. }) => {
            // A COMMIT/END that ran in an aborted tx was rolled back — tag it so.
            let verb = sql.split_ascii_whitespace().next().unwrap_or("");
            let tag = if was_aborted
                && (verb.eq_ignore_ascii_case("commit") || verb.eq_ignore_ascii_case("end"))
            {
                "ROLLBACK".to_string()
            } else {
                command_tag(sql, affected)
            };
            send_command_complete(wbuf, &tag)?;
            // Sync tx state from engine after writes.
            *tx_state = if state.engine.read().is_ok_and(|e| e.is_tx_open(conn_state.tx_id)) {
                b'T'
            } else {
                b'I'
            };
        }
        Err(e) => {
            // v7.17.0 Phase 2.3 — map `Cancelled` to
            // SQLSTATE `57014` so PG client libraries
            // surface it as a statement-timeout, not a
            // generic `42000` syntax / access error.
            let (sqlstate, msg) = engine_error_to_wire_conn(&e, conn_state);
            send_error_pos(wbuf, sqlstate, &msg, parse_error_position(&e, sql))?;
            // After an error inside a TX, PG goes to 'E'
            // and stays there until ROLLBACK. We track
            // best-effort: if engine still in TX, mark
            // 'E'; otherwise 'I'.
            *tx_state = if state.engine.read().is_ok_and(|e| e.is_tx_open(conn_state.tx_id)) {
                b'E'
            } else {
                b'I'
            };
        }
        // v7.5.0 — QueryResult is #[non_exhaustive].
        Ok(_) => {
            send_error(wbuf, "XX000", "unexpected QueryResult variant")?;
        }
    }
    send_ready_for_query(wbuf, *tx_state)?;
    stream.write_all(wbuf)?;
    wbuf.clear();
    Ok(())
}

/// v7.37.43-T4 — dispatch a single statement from a multi-statement
/// `sqlx::migrate!()` script. Writes CC / error / RowDescription /
/// DataRow frames into `wbuf`, NO ReadyForQuery, NO TCP flush. The
/// caller (`dispatch_pg_simple_query_multi`) accumulates all sub-
/// statements into one wbuf and emits one final RFQ + flush per
/// PG protocol §53.2.2.
///
/// Covers the subset that `sqlx::migrate!()` scripts use:
///   * SET / SHOW (with the same engine-affecting / multi-assignment
///     fallthrough as the single-stmt path)
///   * canned-probe answers (psql startup queries)
///   * `execute_with_role` for DDL / DML / SELECT (Rows or CommandOk
///     emitted into `wbuf`)
///
/// COPY in the middle of a multi-stmt script gets an SQLSTATE
/// `0A000` error frame — the COPY sub-protocol needs interactive
/// reads/writes against the real TCP stream the captured wbuf
/// can't proxy. (`psql -f` rewrites file-mode COPY as a metacommand,
/// so practical scripts don't hit this.)
///
/// The streaming-SELECT fast path is intentionally not used here
/// — multi-statement migration scripts are not the hot path; the
/// materialised path keeps the helper short and the per-statement
/// state-tracking correct (each sub-stmt's Rows must land before
/// the next sub-stmt's RowDescription).
#[allow(clippy::too_many_arguments)]
fn handle_pg_simple_query_one_into_wbuf(
    sql_bytes: &[u8],
    state: &Arc<ServerState>,
    conn_state: &Arc<crate::ConnState>,
    role: Role,
    tx_state: &mut u8,
    settings: &mut std::collections::HashMap<String, String>,
    wbuf: &mut Vec<u8>,
) -> std::io::Result<()> {
    // Mirror the activity registry update from the single-stmt path
    // so spg_stat_activity surfaces the current substatement.
    let now_us = wallclock_unix_micros();
    conn_state
        .last_query_start_us
        .store(now_us, std::sync::atomic::Ordering::Relaxed);
    if let Ok(mut s) = conn_state.current_sql.write() {
        s.clear();
        match std::str::from_utf8(sql_bytes) {
            Ok(valid) => s.push_str(valid),
            Err(_) => s.push_str(&String::from_utf8_lossy(sql_bytes)),
        }
    }
    let Ok(sql_str) = std::str::from_utf8(sql_bytes) else {
        send_error(wbuf, "22021", "invalid UTF-8 in query")?;
        return Ok(());
    };
    let sql: &str = sql_str.trim_end_matches(';').trim();
    if sql.is_empty() {
        // Empty statement after splitter trim — emit the PG
        // canonical EmptyQueryResponse marker via CommandComplete
        // with no tag (PG uses 'I' message for empty; CC with empty
        // tag is the closest portable equivalent SPG handles).
        send_command_complete(wbuf, "")?;
        return Ok(());
    }
    // SET name=value — same dispatch as the single-stmt path (v7.39
    // GUC: dual-write the wire cache and ALWAYS fall through to the
    // engine so session_params stays in sync; the pre-7.39 early
    // return here made multi-statement scripts' SETs invisible to
    // the engine while the single-stmt path was already fixed).
    if let Some((name, value)) = parse_set_statement(sql) {
        let name_lc = name.to_ascii_lowercase();
        settings.insert(name_lc.clone(), value.clone());
        if name_lc == "application_name" {
            if let Ok(mut g) = conn_state.application_name.write() {
                *g = value.clone();
            }
        }
    }
    // RESET — keep the wire cache in sync and fall through (engine
    // ResetParameter clears session_params). Same shape as the
    // single-stmt path, including the startup application_name
    // restore.
    {
        let t = sql.trim();
        let b = t.as_bytes();
        let restore_app_name = |settings: &mut std::collections::HashMap<String, String>| {
            if !conn_state.startup_app_name.is_empty() {
                settings.insert(
                    "application_name".to_string(),
                    conn_state.startup_app_name.clone(),
                );
            }
            if let Ok(mut g) = conn_state.application_name.write() {
                g.clone_from(&conn_state.startup_app_name);
            }
        };
        // v7.39 (round 320, V53) — see the single-statement path: DISCARD
        // ALL includes RESET ALL and must drop the wire cache with it.
        if ci_eq(b, b"reset all") || ci_starts_with(b, b"discard all") {
            settings.clear();
            restore_app_name(settings);
        } else if ci_starts_with(b, b"reset ") {
            let name = t[6..]
                .trim()
                .trim_end_matches(';')
                .trim()
                .to_ascii_lowercase();
            settings.remove(&name);
            if name == "application_name" {
                restore_app_name(settings);
            }
        }
    }
    if let Some(name) = parse_show_statement(sql) {
        // v7.39 (GUC) — engine store first, same as the single-stmt path.
        let engine_val: Option<String> = if name != "all" {
            state
                .engine
                .read()
                .ok()
                .and_then(|e| e.session_param(&name).map(str::to_string))
        } else {
            None
        };
        let resp = match engine_val {
            Some(v) => CannedResponse::Rows {
                columns: vec![ColumnSchema::new(name.clone(), DataType::Text, false)],
                rows: vec![Row::new(vec![Value::text(v)])],
            },
            None => render_show(&name, settings),
        };
        send_canned(wbuf, &resp)?;
        return Ok(());
    }
    if parse_copy_intent(sql).is_some() {
        send_error(
            wbuf,
            "0A000",
            "COPY is not supported within a multi-statement simple-query script; \
             send COPY as its own Query message",
        )?;
        return Ok(());
    }
    if let Some(canned) = canned_response(sql, state) {
        send_canned(wbuf, &canned)?;
        return Ok(());
    }
    // v7.39 (query cancel) — arm this statement with the session's
    // CancelRequest flag (cleared first: an idle-time cancel must not
    // kill the NEXT statement, matching PG).
    conn_state
        .cancel_flag
        .store(false, std::sync::atomic::Ordering::Relaxed);
    let cancel = statement_cancel(settings, &conn_state.cancel_flag);
    conn_state
        .wait_event
        .store(1, std::sync::atomic::Ordering::Relaxed);
    // r178 — same commit-barrier routing as the single-statement
    // handler; per-statement of a multi-statement script each split
    // statement is a candidate on its own.
    let (result, queue_persisted) =
        match try_queue_plain_dml(state, sql, role, *tx_state, settings) {
            Some(r) => (r, true),
            None => (
                execute_with_role(
                    state,
                    sql,
                    role,
                    cancel,
                    matches!(*tx_state, b'T' | b'E'),
                    conn_state.tx_id,
                    settings,
                ),
                false,
            ),
        };
    conn_state
        .wait_event
        .store(0, std::sync::atomic::Ordering::Relaxed);
    drain_notices(state, wbuf)?;
    drain_notifications(state, wbuf, conn_state)?;
    let result = if queue_persisted {
        result
    } else {
        match persist_wire_write(state, sql, &result, conn_state.tx_id) {
            Ok(()) => result,
            Err(e) => Err(EngineError::Unsupported(format!(
                "durability append failed: {e}"
            ))),
        }
    };
    match result {
        Ok(QueryResult::Rows { columns, rows }) => {
            send_row_description(wbuf, &columns)?;
            let mat_arena = bumpalo::Bump::new();
            let (wire_style, wire_tz) = state
                .engine
                .read()
                .map(|e| (e.render_style(), e.session_tz()))
                .unwrap_or((Default::default(), spg_engine::SessionTz::Utc));
            for row in &rows {
                encode_data_row(wbuf, &columns, row, &mat_arena, &wire_style, &wire_tz)?;
            }
            // v7.39 (round 131) — RETURNING keeps its DML tag; SELECT tags `SELECT n`.
            send_command_complete(wbuf, &command_tag_for_rows_sql(sql, rows.len()))?;
        }
        Ok(QueryResult::CommandOk { affected, .. }) => {
            let tag = command_tag(sql, affected);
            send_command_complete(wbuf, &tag)?;
            *tx_state = if state.engine.read().is_ok_and(|e| e.is_tx_open(conn_state.tx_id)) {
                b'T'
            } else {
                b'I'
            };
        }
        Err(e) => {
            let (sqlstate, msg) = engine_error_to_wire_conn(&e, conn_state);
            send_error_pos(wbuf, sqlstate, &msg, parse_error_position(&e, sql))?;
            *tx_state = if state.engine.read().is_ok_and(|e| e.is_tx_open(conn_state.tx_id)) {
                b'E'
            } else {
                b'I'
            };
        }
        Ok(_) => {
            send_error(wbuf, "XX000", "unexpected QueryResult variant")?;
        }
    }
    Ok(())
}

/// v7.37.43-T4 — drive a multi-statement `sqlx::migrate!()` script
/// through `handle_pg_simple_query_one_into_wbuf` per statement,
/// then emit exactly one ReadyForQuery + one TCP flush per PG
/// protocol §53.2.2.
///
/// PG's behavior on error in a mid-script statement (when the
/// client did NOT wrap the script in BEGIN…COMMIT itself) is to
/// stop processing further statements and let the client see the
/// error frame followed by RFQ in error state. We mirror that: if
/// any sub-stmt's wbuf encoding ended in an Error frame (tx_state
/// went to 'E'), stop dispatching the remainder.
#[allow(clippy::too_many_arguments)]
fn dispatch_pg_simple_query_multi(
    stream: &mut dyn ReadWrite,
    stmts: &[&[u8]],
    state: &Arc<ServerState>,
    conn_state: &Arc<crate::ConnState>,
    role: Role,
    tx_state: &mut u8,
    settings: &mut std::collections::HashMap<String, String>,
    wbuf: &mut Vec<u8>,
) -> std::io::Result<()> {
    for stmt in stmts {
        let pre_len = wbuf.len();
        handle_pg_simple_query_one_into_wbuf(
            stmt, state, conn_state, role, tx_state, settings, wbuf,
        )?;
        // PG halts a multi-stmt script after the first error frame
        // (the client sees Error + RFQ, no further frames). We
        // detect by checking whether tx_state was bumped to 'E' OR
        // the most recently emitted message starts with 'E' (Error
        // response). The tx_state path covers errors inside a TX
        // started by an earlier statement in the script; the wbuf
        // probe covers errors outside any TX (tx_state stays 'I').
        if *tx_state == b'E' {
            break;
        }
        if let Some(&first_byte_of_last_msg) = wbuf.get(pre_len) {
            // Conservative — only break on Error responses, not on
            // NoticeResponse ('N'); we don't emit NoticeResponses
            // from this path so 'E' uniquely identifies errors.
            if first_byte_of_last_msg == b'E' {
                break;
            }
        }
    }
    send_ready_for_query(wbuf, *tx_state)?;
    stream.write_all(wbuf)?;
    wbuf.clear();
    Ok(())
}

/// New pgwire connection: negotiate optional TLS (SSLRequest), then run the
/// session over the plain or TLS-wrapped stream.
fn handle_conn(mut stream: TcpStream, state: &Arc<ServerState>) -> std::io::Result<()> {
    let _ = stream.set_nodelay(true);
    // v7.39 (round 318, V51) — a shutdown handle for pg_terminate_backend.
    // Cloned before the stream is borrowed by the TLS wrapper.
    let sock = stream.try_clone().ok();
    // v7.39 (TLS) — SSLRequest negotiation. Peek the 8-byte header without
    // consuming it, so a plain-TCP client's StartupMessage is left intact for
    // read_startup; only a real SSLRequest (proto 80877103) is consumed and
    // upgraded. GSSENCRequest (80877104) is declined, then we re-peek.
    loop {
        match peek_startup_proto(&stream)? {
            Some(80877103) => {
                let mut hdr = [0u8; 8];
                stream.read_exact(&mut hdr)?;
                stream.write_all(b"S")?;
                let mut tls_conn =
                    crate::mysqlwire::build_server_connection().map_err(std::io::Error::other)?;
                let mut tls = rustls::Stream::new(&mut tls_conn, &mut stream);
                return run_pg_session(&mut tls, state, true, sock);
            }
            Some(80877104) => {
                let mut hdr = [0u8; 8];
                stream.read_exact(&mut hdr)?;
                stream.write_all(b"N")?;
            }
            // v7.39 (query cancel) — CancelRequest: 16 bytes total
            // (len, code, pid, secret). Trip the target session's
            // cancel flag when the secret matches, then close with
            // no response (PG replies nothing on this connection).
            Some(80877102) => {
                let mut pkt = [0u8; 16];
                stream.read_exact(&mut pkt)?;
                let pid = u32::from_be_bytes([pkt[8], pkt[9], pkt[10], pkt[11]]);
                let secret = u32::from_be_bytes([pkt[12], pkt[13], pkt[14], pkt[15]]);
                if let Ok(conns) = state.connections.read()
                    && let Some(c) = conns.iter().find(|c| c.pid == pid)
                    && c.cancel_secret == secret
                {
                    c.cancel_flag
                        .store(true, std::sync::atomic::Ordering::Relaxed);
                }
                return Ok(());
            }
            _ => return run_pg_session(&mut stream, state, false, sock),
        }
    }
}

/// v7.39 (TLS) — whether the server refuses plaintext (non-TLS) pgwire
/// connections (`SPG_REQUIRE_TLS` set to a truthy value). Cached once.
fn require_tls() -> bool {
    static REQUIRE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *REQUIRE.get_or_init(|| {
        std::env::var("SPG_REQUIRE_TLS")
            .map(|v| {
                matches!(
                    v.trim().to_ascii_lowercase().as_str(),
                    "1" | "true" | "yes" | "on"
                )
            })
            .unwrap_or(false)
    })
}

/// v7.39 (TLS) — peek the first 8 bytes (length + request code) WITHOUT
/// consuming them via the safe `TcpStream::peek` (MSG_PEEK under the hood), so
/// a plain StartupMessage stays buffered for `read_startup`. Returns the
/// request code, or `None` when fewer than 8 bytes are visible (short read /
/// EOF), which the caller treats as "not an SSL/GSS request". Clients send the
/// fixed 8-byte SSLRequest as one segment, so a full peek is reliable.
fn peek_startup_proto(stream: &TcpStream) -> std::io::Result<Option<u32>> {
    let mut buf = [0u8; 8];
    let n = stream.peek(&mut buf)?;
    if n < 8 {
        return Ok(None);
    }
    Ok(Some(u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]])))
}

/// Run the pgwire session (startup, auth, query loop) over a plain or
/// TLS-wrapped stream. `secure` is true when the stream is TLS-wrapped.
fn run_pg_session(
    stream: &mut dyn ReadWrite,
    state: &Arc<ServerState>,
    secure: bool,
    sock: Option<TcpStream>,
) -> std::io::Result<()> {
    // ---- Startup phase ----
    let (user, params) = read_startup(stream)?;
    // v7.39 (TLS) — SPG_REQUIRE_TLS refuses a plaintext connection. Reported
    // after the startup so the client sees a clean ErrorResponse (SQLSTATE
    // 08P01) rather than a dropped socket.
    if !secure && require_tls() {
        send_error(
            stream,
            "08P01",
            "SSL/TLS connection required (SPG_REQUIRE_TLS is set)",
        )?;
        return Ok(());
    }
    // v7.17 Phase 2.4 — surface `application_name` from the startup
    // params on the per-connection ConnState (read back via
    // `spg_stat_activity.application_name`). Other params
    // (database / options / etc.) we still ignore.
    let startup_app_name = params
        .iter()
        .find_map(|(k, v)| (k == "application_name").then(|| v.clone()))
        .unwrap_or_default();
    // v7.39 (read01 misc.c) — the startup `database` param, so
    // `current_database()` and `pg_stat_activity.datname` name the database
    // this connection asked for. Applied below, once this connection's
    // session exists.
    let startup_db = params
        .iter()
        .find_map(|(k, v)| (k == "database").then(|| v.clone()))
        .filter(|db| {
            !db.is_empty()
                && db
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        })
        .unwrap_or_default();

    // v6.5.2 — register this connection in the activity registry.
    // Removed when `_conn_guard` drops at function exit.
    // v7.39 (round 283) — one TX slot per connection, so two clients can
    // hold open transactions at the same time.
    let conn_tx_id = match state.engine.write() {
        Ok(mut e) => e.alloc_tx_id(),
        Err(_) => spg_engine::IMPLICIT_TX,
    };
    let conn_state = Arc::new(crate::ConnState {
        tx_id: conn_tx_id,
        // v7.39 (round 317, V36) — from the process-wide allocator. The
        // old `process::id() + active_connections` repeated an id as soon
        // as one connection left and another arrived, so two LIVE backends
        // could answer the same `pg_backend_pid()` and a CancelRequest
        // could not name one of them.
        pid: crate::alloc_conn_id(),
        user: user.clone(),
        started_at_us: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_micros() as i64)
            .unwrap_or(0),
        current_sql: std::sync::RwLock::new(String::new()),
        wait_event: std::sync::atomic::AtomicU8::new(0),
        last_query_start_us: std::sync::atomic::AtomicI64::new(0),
        in_transaction: std::sync::atomic::AtomicBool::new(false),
        application_name: std::sync::RwLock::new(startup_app_name.clone()),
        startup_app_name: startup_app_name.clone(),
        cancel_secret: new_cancel_secret(),
        cancel_flag: std::sync::atomic::AtomicBool::new(false),
        terminate: std::sync::atomic::AtomicBool::new(false),
        // v7.39 (round 319, V52) — the peer address, so pg_stat_activity's
        // client_addr / client_port and SHOW PROCESSLIST's Host report who
        // is actually attached instead of NULL / a hardcoded "localhost".
        client_addr: sock.as_ref().and_then(|s| s.peer_addr().ok()),
        database: std::sync::RwLock::new(startup_db.clone()),
        sock,
        notify_queue: std::sync::Mutex::new(Vec::new()),
    });

    // v7.39 (round 319) — install this connection's session BEFORE seeding
    // its login identity and database. Both are session state, and the
    // engine is shared: seeding them ahead of `set_current_session` wrote
    // them into whichever connection's bag happened to be installed, so
    // `current_user` / `current_database()` answered another connection's
    // values — and this connection's own bag, created empty on first use,
    // never got them at all.
    if let Ok(mut e) = state.engine.write() {
        e.set_current_session(conn_state.pid);
        if !user.is_empty() {
            // The REPORTED identity only: privilege semantics still key on
            // an explicit SET ROLE (see Engine::is_superuser), so naming a
            // non-admin login here cannot silently turn a connection into
            // an RLS subject.
            e.set_session_user(&user);
        }
        if !startup_db.is_empty() {
            let _ = e.execute(&format!("SET spg.database = '{startup_db}'"));
        }
    }

    // v7.39 (read01 pgstatfuncs.c) — stamp this connection's pid into the
    // thread-local the engine's pg_backend_pid() slot reads.
    crate::set_conn_pid(conn_state.pid);
    if let Ok(mut conns) = state.connections.write() {
        conns.push(Arc::clone(&conn_state));
    }
    // v7.39 (pg_stat knife A) — pg_stat_database.numbackends mirror
    // (pgwire sessions don't pass through main's ConnectionGuard).
    crate::backend_count_incr();
    // RAII guard: drops the connection from the registry when this
    // function returns (normal exit or error).
    struct ConnGuard {
        state: Arc<ServerState>,
        conn: Arc<crate::ConnState>,
    }
    impl Drop for ConnGuard {
        fn drop(&mut self) {
            if let Ok(mut conns) = self.state.connections.write() {
                conns.retain(|x| !Arc::ptr_eq(x, &self.conn));
            }
            // v7.39 (round 279) — drop this connection's parked session
            // state and release every advisory lock it still holds, the
            // way PG does at backend exit. Without it a crashed client
            // would hold a lock forever in the shared engine.
            if let Ok(mut e) = self.state.engine.write() {
                // v7.39 (round 283) — a connection that vanishes mid
                // transaction must not leave its shadow slot behind. PG
                // aborts the backend's transaction at exit; without this
                // the slot (and its uncommitted writes) would sit in
                // `tx_catalogs` for the life of the process.
                if e.is_tx_open(self.conn.tx_id) {
                    let _ = e.execute_in("ROLLBACK", self.conn.tx_id);
                }
                e.end_session(self.conn.pid);
            }
            crate::backend_count_decr();
        }
    }
    let _conn_guard = ConnGuard {
        state: Arc::clone(state),
        conn: Arc::clone(&conn_state),
    };
    // RBAC: if there are users in the engine, demand password.
    // Else (open mode), accept any startup as admin.
    let has_users = state.engine.read().is_ok_and(|e| !e.users().is_empty());

    let role = if has_users {
        // v4.8: prefer SCRAM-SHA-256 when the user has stored
        // secrets. Fall back to CleartextPassword for legacy users
        // (loaded from a pre-v4.8 snapshot, no SCRAM verifier on
        // file). Modern PG drivers (JDBC, asyncpg, psycopg3) refuse
        // cleartext over plain TCP unless explicitly opted in;
        // SCRAM keeps them happy.
        let user_has_scram = state
            .engine
            .read()
            .ok()
            .and_then(|e| {
                e.users()
                    .iter()
                    .find_map(|(n, r)| (n == user).then(|| r.scram().is_some()))
            })
            .unwrap_or(false);
        let outcome = if user_has_scram {
            scram_auth(stream, state, &user, secure)?
        } else {
            cleartext_auth(stream, state, &user)?
        };
        match outcome {
            Some(r) => r,
            None => return Ok(()), // error already sent
        }
    } else {
        Role::Admin
    };

    // AuthenticationOk
    send_msg(stream, b'R', &0u32.to_be_bytes())?;
    // ParameterStatus pairs — keep the set minimal but include the
    // ones psql / driver libraries check first.
    send_parameter_status(stream, "server_version", "18.4 (spg-4.3)")?;
    send_parameter_status(stream, "client_encoding", "UTF8")?;
    send_parameter_status(stream, "DateStyle", "ISO, MDY")?;
    send_parameter_status(stream, "integer_datetimes", "on")?;
    send_parameter_status(stream, "standard_conforming_strings", "on")?;
    // v7.39 (query cancel) — BackendKeyData carries the REAL
    // (pid, secret) pair; a CancelRequest connection echoing them
    // trips this session's cancel flag mid-statement.
    let mut bkd = Vec::with_capacity(8);
    bkd.extend_from_slice(&conn_state.pid.to_be_bytes());
    bkd.extend_from_slice(&conn_state.cancel_secret.to_be_bytes());
    send_msg(stream, b'K', &bkd)?;
    send_ready_for_query(stream, b'I')?;

    // ---- Query loop ----
    let mut tx_state = b'I'; // 'I' idle / 'T' in transaction / 'E' failed
    // v4.7: extended-query state. Anonymous statement / portal use
    // empty-string names per PG spec. Named statements survive
    // until explicitly Closed (`C` message) or the connection ends.
    let mut prepared: std::collections::HashMap<String, PreparedStmt> =
        std::collections::HashMap::default();
    let mut portals: std::collections::HashMap<String, Portal> =
        std::collections::HashMap::default();
    // v4.19: per-connection SET / SHOW state. PG clients SET
    // application_name, client_encoding, search_path, etc. at
    // startup; we accept and remember them. SHOW reads from this
    // map first, falls back to a known-defaults table.
    let mut settings: std::collections::HashMap<String, String> =
        std::collections::HashMap::default();
    // v7.17 Phase 2.4 — seed the per-session SHOW map with whatever
    // the client declared in the startup `application_name` param so
    // `SHOW application_name` returns the right value without a
    // prior SET. Empty string is fine; that matches PG's behaviour
    // for unset GUCs.
    if !startup_app_name.is_empty() {
        settings.insert("application_name".to_string(), startup_app_name.clone());
    }
    // v6.3.2 — pipelined-query response buffer. Every send_*
    // helper writes here instead of straight to the socket; the
    // buffer is flushed at strategic sync points:
    //   - after each simple-query 'Q' that ends with ReadyForQuery
    //   - on extended-query 'S' (Sync) / 'H' (Flush)
    //   - before COPY mode hands the raw stream to its handler
    //   - if the buffer grows past PIPELINE_FLUSH_BYTES (4 KiB) —
    //     defensive backstop against a client that piles up
    //     responses without ever sending Sync
    // For pipelined batches of N P/B/E messages followed by S, the
    // server now hands the kernel one write() per Sync instead of
    // 3N syscalls. Loopback already coalesces well via Nagle —
    // this brings the same property to high-latency networks.
    const PIPELINE_FLUSH_BYTES: usize = 4096;
    let mut wbuf: Vec<u8> = Vec::with_capacity(8192);
    // v7.36 (perf — mailrs Phase 1 wire) — reusable inbound message
    // buffer. Pre-7.36 every protocol message allocated a fresh
    // `Vec<u8>` for the body (Bind / Execute / Sync = 3-4 fresh
    // allocs per prepared query); now we grow once and reuse,
    // resizing the buffer in place for each message. Same `&[u8]`
    // borrow shape downstream — handlers don't notice.
    let mut rbuf: Vec<u8> = Vec::with_capacity(8192);
    // v7.37.x (SPGS PLUCK 红线 — speculative read) — try to fetch
    // header + body in one syscall for small messages. PG protocol
    // header (5 bytes) plus a short body (e.g. `SELECT 1\0` = 9 B)
    // fits in a small read; psql normally TCP-packs the whole query
    // into one segment. The first `read()` call here pulls up to 256
    // bytes and parses the header from the front — when the body
    // fits in the same chunk the second `read_exact(&mut rbuf)`
    // syscall is skipped, halving the per-query I/O system-call
    // count (~15-20 µs / syscall on macOS).
    let mut peek_buf = [0u8; 256];
    let mut peek_have: usize = 0;
    loop {
        // v7.39 (round 318, V51) — `pg_terminate_backend` on this
        // connection. PG answers the terminated backend with a FATAL and
        // closes; checking here (and on EOF below) covers both a
        // connection parked in read — whose read half the signal shut
        // down, waking it — and one that just finished a statement.
        if terminated(&conn_state) {
            let _ = stream.write_all(&wbuf);
            wbuf.clear();
            return send_fatal_terminated(stream);
        }
        // Ensure we have at least 5 bytes for the header. The
        // speculative read pulls up to peek_buf.len() bytes; refills
        // from the socket only when the buffer doesn't already hold
        // a full header.
        while peek_have < 5 {
            let n = match stream.read(&mut peek_buf[peek_have..]) {
                Ok(n) => n,
                Err(e) => {
                    if e.kind() == std::io::ErrorKind::UnexpectedEof {
                        return if terminated(&conn_state) {
                            send_fatal_terminated(stream)
                        } else {
                            Ok(())
                        };
                    }
                    return Err(e);
                }
            };
            if n == 0 {
                return if terminated(&conn_state) {
                    send_fatal_terminated(stream)
                } else {
                    Ok(())
                };
            }
            peek_have += n;
        }
        let msg_type = peek_buf[0];
        let len = u32::from_be_bytes([peek_buf[1], peek_buf[2], peek_buf[3], peek_buf[4]]) as usize;
        // PG length includes the 4 bytes of the length itself.
        let body_len = len.saturating_sub(4);
        let in_peek = peek_have - 5;
        if body_len <= in_peek {
            // Body fully present in the speculative read. Copy out
            // for downstream borrow (rbuf needs stable lifetime
            // across the dispatch arms below) and shift any
            // remaining bytes to the front of peek_buf for the next
            // iteration.
            rbuf.resize(body_len, 0);
            rbuf[..body_len].copy_from_slice(&peek_buf[5..5 + body_len]);
            let leftover = peek_have - 5 - body_len;
            if leftover > 0 {
                peek_buf.copy_within(5 + body_len..peek_have, 0);
            }
            peek_have = leftover;
        } else {
            // Body extends past the peek chunk. Move what we have
            // into rbuf and read the remainder via the legacy path.
            rbuf.resize(body_len, 0);
            rbuf[..in_peek].copy_from_slice(&peek_buf[5..peek_have]);
            stream.read_exact(&mut rbuf[in_peek..])?;
            peek_have = 0;
        }
        let body: &[u8] = &rbuf;

        match msg_type {
            b'Q' => handle_pg_simple_query(
                stream,
                body,
                state,
                &conn_state,
                role,
                &mut tx_state,
                &mut settings,
                &mut wbuf,
            )?,
            b'X' => {
                // Terminate. Flush any pending bytes before returning so
                // a CommandComplete on the last simple query doesn't
                // get dropped by the connection teardown.
                if !wbuf.is_empty() {
                    let _ = stream.write_all(&wbuf);
                }
                return Ok(());
            }
            // ---- v4.7: extended-query protocol ----
            // Parse (P): name + SQL + parameter type OIDs. Store the
            // statement; reply ParseComplete (no ReadyForQuery — that
            // waits for Sync).
            b'P' => {
                if let Err(msg) = handle_parse(body, &mut prepared, state) {
                    send_error(&mut wbuf, "42601", &msg)?;
                } else {
                    send_msg(&mut wbuf, b'1', &[])?;
                }
            }
            // Bind (B): create a portal with parameter values
            // substituted into the prepared statement's SQL.
            b'B' => {
                match handle_bind(body, &prepared) {
                    Ok(portal) => {
                        portals.insert(portal.0.clone(), portal.1);
                        send_msg(&mut wbuf, b'2', &[])?; // BindComplete
                    }
                    Err(msg) => send_error(&mut wbuf, "42601", &msg)?,
                }
            }
            // Describe (D): describe statement ('S') or portal ('P').
            // For statements we send ParameterDescription + NoData (we
            // don't dry-run-parse to discover row shape). For portals
            // we likewise send NoData; the real RowDescription
            // arrives after Execute via the regular row stream.
            b'D' => {
                if !body.is_empty() {
                    let kind = body[0];
                    let name = cstring_at(body, 1).unwrap_or_default();
                    // v6.3.3 — real Describe. Statement (S) returns
                    // ParameterDescription + RowDescription | NoData.
                    // Portal (P) returns RowDescription | NoData
                    // (portals don't carry their own param desc — that's
                    // on the underlying statement).
                    let (param_oids, columns): (Vec<u32>, Vec<ColumnSchema>) = if kind == b'S' {
                        if let Some(stmt) = prepared.get(&name) {
                            let eng = state
                                .engine
                                .read()
                                .map_err(|_| std::io::Error::other("engine lock poisoned"))?;
                            eng.describe_prepared(&stmt.ast)
                        } else {
                            (Vec::new(), Vec::new())
                        }
                    } else if kind == b'P' {
                        let cols = if let Some(portal) = portals.get(&name) {
                            if let Some(stmt) = prepared.get(&portal.stmt_name) {
                                let eng = state
                                    .engine
                                    .read()
                                    .map_err(|_| std::io::Error::other("engine lock poisoned"))?;
                                let (_, c) = eng.describe_prepared(&stmt.ast);
                                c
                            } else {
                                Vec::new()
                            }
                        } else {
                            Vec::new()
                        };
                        (Vec::new(), cols)
                    } else {
                        (Vec::new(), Vec::new())
                    };
                    if kind == b'S' {
                        let n = u16::try_from(param_oids.len())
                            .map_err(|_| std::io::Error::other("too many parameters"))?;
                        let mut pd = Vec::with_capacity(2 + param_oids.len() * 4);
                        pd.extend_from_slice(&n.to_be_bytes());
                        for oid in &param_oids {
                            pd.extend_from_slice(&oid.to_be_bytes());
                        }
                        send_msg(&mut wbuf, b't', &pd)?;
                    }
                    if columns.is_empty() {
                        send_msg(&mut wbuf, b'n', &[])?; // NoData
                    } else {
                        send_row_description(&mut wbuf, &columns)?;
                    }
                }
            }
            // Execute (E): portal name + max-rows (0 = all).
            b'E' => {
                if let Err((sqlstate, msg)) = handle_execute(
                    body,
                    &mut portals,
                    &prepared,
                    &settings,
                    &mut wbuf,
                    state,
                    role,
                    &mut tx_state,
                    &conn_state,
                ) {
                    send_error(&mut wbuf, sqlstate, &msg)?;
                }
            }
            // Close (C): drop the named statement or portal. Reply
            // CloseComplete.
            b'C' => {
                if body.len() >= 2 {
                    let kind = body[0];
                    let name = cstring_at(body, 1).unwrap_or_default();
                    if kind == b'S' {
                        prepared.remove(&name);
                    } else if kind == b'P' {
                        portals.remove(&name);
                    }
                }
                send_msg(&mut wbuf, b'3', &[])?; // CloseComplete
            }
            // Flush (H): client wants pending responses on the wire
            // without forcing a Sync (which would also emit
            // ReadyForQuery). Drain wbuf to the socket.
            b'H' => {
                if !wbuf.is_empty() {
                    stream.write_all(&wbuf)?;
                    wbuf.clear();
                }
            }
            // Sync (S): boundary marker — reply with ReadyForQuery
            // reflecting the current transaction state, then drain
            // every accumulated response in one syscall (the v6.3.2
            // pipelining win).
            b'S' => {
                send_ready_for_query(&mut wbuf, tx_state)?;
                stream.write_all(&wbuf)?;
                wbuf.clear();
            }
            // CopyData / CopyDone / CopyFail outside of an active
            // COPY block — protocol error from the client.
            b'd' | b'c' | b'f' => {
                send_error(
                    &mut wbuf,
                    "08P01",
                    "unexpected CopyData/Done/Fail outside COPY mode",
                )?;
                send_ready_for_query(&mut wbuf, tx_state)?;
                stream.write_all(&wbuf)?;
                wbuf.clear();
            }
            _ => {
                send_error(
                    &mut wbuf,
                    "08P01",
                    &format!("unknown frontend message type: 0x{msg_type:02x}"),
                )?;
                send_ready_for_query(&mut wbuf, tx_state)?;
                stream.write_all(&wbuf)?;
                wbuf.clear();
            }
        }
        // Defensive backstop: if a client piles up many P/B/E without
        // ever sending Sync, drain the buffer once it crosses the
        // 4 KiB threshold so the client receiving these responses
        // can make forward progress.
        if wbuf.len() >= PIPELINE_FLUSH_BYTES {
            stream.write_all(&wbuf)?;
            wbuf.clear();
        }
    }
}

/// v7.39 (round 178) — route a plain autocommit DML through the commit
/// barrier so pgwire writes share the native wrap path's group fsync
/// (concurrent writers coalesce into one `sync_data`). Returns `None`
/// when the statement must take the classic inline path:
///   * inside a transaction (r177 already made those fsync-free; the
///     COMMIT is the durability point),
///   * no WAL,
///   * not a bare INSERT/UPDATE/DELETE/MERGE (DDL, SET, WITH-DML and
///     multi-statement scripts keep the inline path),
///   * RETURNING present (the leader treats a Rows result as a failed
///     slot — see run_leader_commit_round step 2),
///   * a session statement_timeout is set (it rides a deadline token
///     the queue can't carry; the flag-based cancel still works).
fn try_queue_plain_dml(
    state: &Arc<ServerState>,
    sql: &str,
    role: Role,
    tx_state: u8,
    settings: &std::collections::HashMap<String, String>,
) -> Option<Result<QueryResult, EngineError>> {
    if tx_state != b'I' || state.wal.is_none() {
        return None;
    }
    let verb = sql
        .trim_start()
        .split_ascii_whitespace()
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    if !matches!(verb.as_str(), "insert" | "update" | "delete" | "merge") {
        return None;
    }
    let b = sql.as_bytes();
    if ci_contains(b, b"returning") {
        return None;
    }
    // Multi-statement scripts must not reach execute_in as one text.
    if sql.trim_end().trim_end_matches(';').contains(';') {
        return None;
    }
    let timeout_set = settings
        .get("statement_timeout")
        .map(String::as_str)
        .and_then(parse_timeout_ms)
        .unwrap_or(0)
        > 0;
    if timeout_set {
        return None;
    }
    // Role gate — the inline path's check lives in execute_with_role,
    // which this route bypasses.
    if !role.can_write() {
        return Some(Err(EngineError::Unsupported(
            "permission denied: write requires admin or readwrite role".into(),
        )));
    }
    // The queue's cancel flag is per-task (the native watchdog owns
    // it there). ConnState's session flag isn't Arc-shaped and no
    // watchdog thread exists on this path (timeout sessions were
    // excluded above); a CancelRequest racing a sub-ms queued DML
    // completing is within PG's cancel contract. Pass a fresh flag.
    let queue_flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (result, wal_outcome) = crate::commit_queue_execute(state, sql.to_string(), &queue_flag);
    if let Err(e) = wal_outcome {
        return Some(Err(EngineError::Unsupported(format!(
            "durability append failed: {e}"
        ))));
    }
    // Audit parity with persist_wire_write (the WAL append itself
    // already happened inside the leader's group).
    if matches!(&result, Ok(QueryResult::CommandOk { .. }))
        && state.audit_path.is_some()
        && let Err(e) = crate::append_audit_pub(state, sql)
    {
        return Some(Err(EngineError::Unsupported(format!(
            "audit append failed: {e}"
        ))));
    }
    Some(result)
}

fn execute_with_role(
    state: &Arc<ServerState>,
    sql: &str,
    role: Role,
    cancel: CancelToken<'_>,
    // v7.39 (read01 round 84) — whether THIS connection is inside a transaction
    // (per-connection tx_state, not the engine's global flag). A SELECT here
    // must then take the write path so it sees the transaction's own writes.
    conn_in_tx: bool,
    // v7.39 (round 283) — the connection's own TX slot.
    tx_id: spg_engine::TxId,
    // v7.39 (round 299) — session GUCs; `lock_timeout` bounds the
    // blocking `FOR UPDATE` wait.
    settings: &std::collections::HashMap<String, String>,
) -> Result<QueryResult, EngineError> {
    // v5.1: cold-tier preload — kept symmetric with the native
    // Op::Query path so a sweep that drives the server through
    // PG-wire still triggers `try_lazy_preload_cold`.
    crate::try_lazy_preload_cold(state);
    // Reuse the same gating ideas as the native wire dispatch:
    // SELECT / SHOW take the read lock; everything else takes the
    // write lock. Role enforcement lives in this helper so the
    // PG-wire shim doesn't have to peek SQL twice.
    let lower_first = sql
        .trim_start()
        .split_ascii_whitespace()
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    let mut is_read = matches!(lower_first.as_str(), "select" | "show");
    // v7.17 dump-compat — `SELECT setval(...)` /
    // `SELECT nextval(...)` mutate sequence state. The
    // readonly path takes `&self` so its pre-resolve hook
    // can't fire — drop the read fast-path when those calls
    // are present so the write path runs them in
    // pre_resolve_sequence_calls_in_statement.
    // v7.34.5 — byte-level case-insensitive substring probe instead
    // of `sql.to_ascii_lowercase()` + three `.contains()` walks.
    // Every SELECT on the hot path paid one whole-SQL String alloc
    // + four full-length scans for a clause virtually no SPG
    // workload uses (sequence mutators inside SELECT only show up
    // in `pg_dump` restores). The byte probes short-circuit on the
    // first non-match without ever building the lowercase copy.
    if is_read {
        let b = sql.as_bytes();
        if ci_contains(b, b"setval(") || ci_contains(b, b"nextval(") || ci_contains(b, b"currval(")
            // v7.39 (GUC) — set_config writes the session GUC store, so
            // it needs the &mut engine path for the same reason.
            || ci_contains(b, b"set_config(")
            // v7.39 (round 295, E3 Phase 1b) — a SELECT that asks for row
            // locks MUTATES the lock table, so it is not a read. Without
            // this it went to the read-only executor, the locking
            // pre-pass never ran, and `FOR UPDATE` was silently ignored
            // in autocommit — which is exactly where a queue worker
            // runs it.
            || ci_contains(b, b" for update")
            || ci_contains(b, b" for share")
            || ci_contains(b, b" for no key update")
            || ci_contains(b, b" for key share")
        {
            is_read = false;
        }
    }
    if !is_read && !role.can_write() {
        return Err(EngineError::Unsupported(
            "permission denied: write requires admin or readwrite role".into(),
        ));
    }
    // CREATE/DROP USER need admin.
    let is_user_mgmt = (lower_first == "create" || lower_first == "drop")
        && sql
            .split_ascii_whitespace()
            .nth(1)
            .is_some_and(|w| w.eq_ignore_ascii_case("user"));
    if is_user_mgmt && !role.can_manage_users() {
        return Err(EngineError::Unsupported(
            "permission denied: user management requires admin role".into(),
        ));
    }
    // v7.39 (read01 round 84) — the read-only `&self` executor reads the
    // COMMITTED base tables; it does not consult an open transaction's
    // uncommitted working set (that lives in the `&mut` executor's transaction
    // buffer). So inside a `BEGIN … ` block a SELECT routed to the read path
    // could not see the transaction's own INSERT / UPDATE / DELETE —
    // read-your-own-writes was broken over the wire, while the embedded API
    // (which always uses the `&mut` execute path) saw them and passed its tests.
    // In a transaction, take the write lock so the SELECT runs through the
    // executor that threads the transaction snapshot. Autocommit reads keep the
    // shared read lock (correct: they see committed data, and stay parallel).
    if is_read && !conn_in_tx {
        let engine = state
            .engine
            .read()
            .map_err(|_| EngineError::Unsupported("engine rwlock poisoned".into()))?;
        engine.execute_readonly_with_cancel(sql, cancel)
    } else {
        // v7.39 (round 299, E3 Phase 2) — the blocking `FOR UPDATE`
        // wait lives HERE, outside the engine lock.
        //
        // PG blocks until the holder commits, then proceeds. SPG cannot
        // block inside the engine write lock: that would stop every
        // connection, including the one whose COMMIT releases the row —
        // the wait would deadlock the server against itself. So the
        // engine reports `LockWouldBlock` and the retry happens after
        // the guard drops.
        //
        // Re-attempting also re-registers the wait edge, which is what
        // lets the lock manager's cycle detector fire: two transactions
        // waiting on each other resolve to 40P01 instead of spinning.
        let deadline = lock_wait_deadline(settings);
        loop {
            let attempt = {
                let mut engine = state
                    .engine
                    .write()
                    .map_err(|_| EngineError::Unsupported("engine rwlock poisoned".into()))?;
                engine.execute_in_with_cancel(sql, tx_id, cancel)
            }; // guard drops here — the holder can now commit
            match attempt {
                Err(EngineError::LockWouldBlock) => {
                    if let Some(d) = deadline
                        && std::time::Instant::now() >= d
                    {
                        return Err(EngineError::Unsupported(
                            "canceling statement due to lock timeout".into(),
                        ));
                    }
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
                other => return other,
            }
        }
    }
}

/// v7.39 (round 299) — when to give up waiting for a row lock.
///
/// `lock_timeout = 0` (PG's default) means wait forever, which is what
/// PG does; `None` here carries that. A cycle still terminates, because
/// each retry re-registers the wait edge and the lock manager's
/// detector answers 40P01.
fn lock_wait_deadline(
    settings: &std::collections::HashMap<String, String>,
) -> Option<std::time::Instant> {
    let raw = settings.get("lock_timeout")?;
    let ms = parse_timeout_ms(raw)?;
    (ms > 0).then(|| std::time::Instant::now() + std::time::Duration::from_millis(ms))
}

/// v7.33 (A1) — persist a successful pgwire write to the durability
/// surface (WAL, else the no-WAL snapshot) and audit, BEFORE the client
/// is told the command completed. Both pgwire entry points — the
/// simple-query path (`execute_with_role`) and the extended-query path
/// (`execute_prepared`) — plus the mysql-wire `COM_QUERY` /
/// `COM_STMT_EXECUTE` handlers — route every write through here so one
/// path can't silently skip durability. Pre-7.33 NONE of the pgwire /
/// mysql-wire paths persisted (only native-wire did): server-mode
/// writes from psql / sqlx / mysql clients were lost on crash. `sql` is
/// the statement text to replay — bind-final (params substituted to
/// literals) on the prepared paths so replay reproduces the effect.
pub(crate) fn persist_wire_write(
    state: &Arc<ServerState>,
    sql: &str,
    result: &Result<QueryResult, EngineError>,
    tx_id: spg_engine::TxId,
) -> std::io::Result<()> {
    // v7.39 (round 178) — a DML with RETURNING answers `Rows`, not
    // `CommandOk`. Pre-r178 this let-else dropped it ("SELECT —
    // nothing to persist"), so `INSERT … RETURNING` over pgwire /
    // mysql-wire NEVER reached the WAL: acked writes vanished on
    // crash (caught by the r178 restart pin). Rows from a DML-shaped
    // text persist like any write; Rows from a real read still skip.
    let modified_catalog = match result {
        Ok(QueryResult::CommandOk {
            modified_catalog, ..
        }) => *modified_catalog,
        Ok(QueryResult::Rows { .. }) if crate::sql_is_dmlish(sql) => true,
        _ => return Ok(()), // read / error — nothing to persist
    };
    let modified_catalog = &modified_catalog;
    if state.wal.is_some() {
        // v7.37.15 (r172) — session synchronous_commit decides whether
        // this append fsyncs. Safe to read here: no engine lock is
        // held (the no-WAL branch below takes engine.read() itself).
        //
        // v7.39 (round 177) — PG only fsyncs at COMMIT points, never
        // per-statement inside an open transaction. If the engine is
        // still in a transaction AFTER this statement (BEGIN, or any
        // statement between BEGIN and COMMIT), the bytes are appended
        // without fsync; the COMMIT statement leaves the transaction
        // → its append fsyncs once, covering the whole tx. Pre-r177
        // an explicit tx over pgwire paid one fsync PER STATEMENT
        // (tx_batch_100 = 100 fsyncs ≈ 740 ms, the panel's 24× worst
        // cell). A crash before the COMMIT fsync loses only the
        // uncommitted tx — exactly PG's contract.
        //
        // v7.39 (round 304, V38) — the witness is THIS connection's
        // slot. `in_transaction()` is true whenever ANY connection
        // holds a transaction, so an autocommit write on one connection
        // skipped its fsync — and was acked — merely because a
        // different connection had a transaction open, breaking
        // synchronous_commit=on. (Same global-vs-slot confusion r298
        // fixed for the aborted flag and pgwire's streaming gate.)
        let in_tx = state
            .engine
            .read()
            .is_ok_and(|e| e.is_tx_open(tx_id));
        crate::append_wal(state, sql, crate::session_sync_commit(state) && !in_tx)?;
    } else if *modified_catalog && state.db_path.is_some() {
        // No-WAL mode: capture the current committed state.
        let bytes = state
            .engine
            .read()
            .map_err(|_| std::io::Error::other("engine rwlock poisoned"))?
            .snapshot();
        if let Some(path) = state.db_path.as_deref() {
            crate::write_atomic(path, &bytes)?;
        }
    }
    if *modified_catalog && state.audit_path.is_some() {
        crate::append_audit_pub(state, sql)?;
    }
    Ok(())
}

fn command_tag(sql: &str, affected: usize) -> String {
    let first = sql
        .trim_start()
        .split_ascii_whitespace()
        .next()
        .unwrap_or("")
        .to_ascii_uppercase();
    match first.as_str() {
        "INSERT" => format!("INSERT 0 {affected}"),
        "UPDATE" => format!("UPDATE {affected}"),
        "DELETE" => format!("DELETE {affected}"),
        "BEGIN" => "BEGIN".to_string(),
        // v7.39 (round 320, V53) — PG tags DISCARD with the target it
        // named: `DISCARD ALL` / `DISCARD PLANS` / …
        "DISCARD" => {
            let target = sql
                .trim_start()
                .split_ascii_whitespace()
                .nth(1)
                .unwrap_or("")
                .trim_end_matches(';')
                .to_ascii_uppercase();
            if target.is_empty() {
                "DISCARD".to_string()
            } else {
                format!("DISCARD {target}")
            }
        }
        "COMMIT" => "COMMIT".to_string(),
        "ROLLBACK" => "ROLLBACK".to_string(),
        // v7.39 (round 219) — cursor command tags. MOVE reports the moved
        // count; CLOSE ALL tags `CLOSE CURSOR ALL` (PG's spelling).
        "DECLARE" => "DECLARE CURSOR".to_string(),
        "MOVE" => format!("MOVE {affected}"),
        "CLOSE" => {
            let second = sql
                .trim_start()
                .split_ascii_whitespace()
                .nth(1)
                .unwrap_or("");
            if second.eq_ignore_ascii_case("all") {
                "CLOSE CURSOR ALL".to_string()
            } else {
                "CLOSE CURSOR".to_string()
            }
        }
        // v7.38 (read01 P3.27) — the first word does not name the command
        // for a data-modifying CTE (`WITH … INSERT/UPDATE/DELETE`) or a
        // MERGE, so PG tags them by the real top-level statement. Parse to
        // recover it; fall back to the keyword if parsing somehow fails
        // (it won't for a statement that already executed).
        "WITH" | "MERGE" => spg_sql::parser::parse_statement(sql)
            .map(|stmt| command_tag_for_ast(&stmt, affected))
            .unwrap_or(first),
        // v7.39 (read01 utils/adt) — PG's DDL tags are "<VERB> <OBJECT>"
        // (CREATE TABLE / DROP INDEX / ALTER TYPE ...). Scan past the
        // modifier words (UNIQUE / OR REPLACE / TEMP / IF EXISTS ...) to
        // the object keyword; USER tags as ROLE (PG's alias), MATERIALIZED
        // as MATERIALIZED VIEW. CREATE MATERIALIZED VIEW is a recorded
        // delta: PG tags it SELECT <n> (the materialised row count).
        first @ ("CREATE" | "ALTER" | "DROP") => {
            let object = sql.trim_start().split_ascii_whitespace().skip(1).find(|w| {
                !w.eq_ignore_ascii_case("unique")
                    && !w.eq_ignore_ascii_case("or")
                    && !w.eq_ignore_ascii_case("replace")
                    && !w.eq_ignore_ascii_case("temp")
                    && !w.eq_ignore_ascii_case("temporary")
                    && !w.eq_ignore_ascii_case("unlogged")
                    && !w.eq_ignore_ascii_case("global")
                    && !w.eq_ignore_ascii_case("local")
                    && !w.eq_ignore_ascii_case("recursive")
                    && !w.eq_ignore_ascii_case("concurrently")
                    // v7.39 (read01 round 82) — CREATE CONSTRAINT TRIGGER tags
                    // as CREATE TRIGGER in PG; skip the CONSTRAINT modifier so
                    // the object keyword (TRIGGER) is what's found.
                    && !w.eq_ignore_ascii_case("constraint")
            });
            const OBJECTS: &[&str] = &[
                "TABLE",
                "INDEX",
                "VIEW",
                "SEQUENCE",
                "SCHEMA",
                "TYPE",
                "EXTENSION",
                "DOMAIN",
                "TRIGGER",
                "FUNCTION",
                "DATABASE",
                "PUBLICATION",
                "SUBSCRIPTION",
                "POLICY",
                "ROLE",
            ];
            match object {
                Some(w) if w.eq_ignore_ascii_case("user") => format!("{first} ROLE"),
                Some(w) if w.eq_ignore_ascii_case("materialized") && first != "CREATE" => {
                    format!("{first} MATERIALIZED VIEW")
                }
                Some(w) => match OBJECTS.iter().find(|o| w.eq_ignore_ascii_case(o)) {
                    Some(o) => format!("{first} {o}"),
                    None => first.to_string(),
                },
                None => first.to_string(),
            }
        }
        "TRUNCATE" => "TRUNCATE TABLE".to_string(),
        "REFRESH" => "REFRESH MATERIALIZED VIEW".to_string(),
        other => other.to_string(),
    }
}

/// Canned answers for client startup probes + the v4.6 pg_catalog
/// subset. Saves us implementing real pg_class / pg_namespace / etc.
/// tables in the engine just to make `psql` and friends not bail on
/// connect. The patterns matched here are exact-prefix lowercased
/// matches; anything stranger drops through to the engine, which
/// will reject pg_catalog table names with a clear "not found"
/// error.
/// v7.34.3 (select_1 SPGS hot-path) — case-insensitive byte-level
/// `starts_with`. Replaces the previous `sql.trim().to_ascii_lowercase()`
/// allocations that each non-canned query paid through every
/// `parse_set_statement` / `parse_show_statement` / `parse_copy_intent`
/// / `canned_response` probe in `handle_pg_simple_query`. Four String
/// allocations + four full-string lowercase walks per query cost ~5-10 µs
/// before the simple-query dispatch ever reached the engine — the
/// dominant contributor to the SPGS `SELECT 1` 113 µs vs PG18 44 µs gap.
fn ci_starts_with(b: &[u8], prefix: &[u8]) -> bool {
    b.len() >= prefix.len() && b[..prefix.len()].eq_ignore_ascii_case(prefix)
}

fn ci_eq(b: &[u8], target: &[u8]) -> bool {
    b.eq_ignore_ascii_case(target)
}

fn ci_strip_prefix<'a>(b: &'a [u8], prefix: &[u8]) -> Option<&'a [u8]> {
    if ci_starts_with(b, prefix) {
        Some(&b[prefix.len()..])
    } else {
        None
    }
}

fn trim_ascii(b: &[u8]) -> &[u8] {
    let mut start = 0;
    while start < b.len() && b[start].is_ascii_whitespace() {
        start += 1;
    }
    let mut end = b.len();
    while end > start && b[end - 1].is_ascii_whitespace() {
        end -= 1;
    }
    &b[start..end]
}

fn canned_response(sql: &str, state: &Arc<ServerState>) -> Option<CannedResponse> {
    let trimmed = sql.trim();
    let b = trimmed.as_bytes();
    // v7.37.x (SPGS PLUCK 红线) — pure-literal SELECTs ("SELECT 1",
    // "SELECT 42", "SELECT NULL") show up in liveness probes,
    // pgbouncer keepalives, ORM connection-pool pings, and BI client
    // canary queries. Pre-7.37.x the round-trip went all the way
    // through SQL parse → engine dispatch → streaming wrap →
    // `encode_data_row`, adding ~65 µs over PG 18's 32 µs wire RT.
    // Bake a fast small-int canned response and use it when the SQL
    // is exactly `SELECT <decimal>` with nothing else.
    if let Some(rest) = ci_strip_prefix(b, b"select ") {
        let rest_trim = trim_ascii(rest);
        if !rest_trim.is_empty()
            && rest_trim
                .iter()
                .all(|c| c.is_ascii_digit() || *c == b'-' || *c == b'+')
            && let Ok(s) = core::str::from_utf8(rest_trim)
            && let Ok(n) = s.parse::<i64>()
        {
            // v7.39 (round 320, V53) — PG types an integer literal that
            // fits in int4 as `integer`, and only a wider one as `bigint`
            // (`SELECT pg_typeof(1), pg_typeof(2147483648)` →
            // `integer|bigint`). This fast path answered int8 for every
            // literal, so the hottest query on the wire — `SELECT 1` —
            // reported a type the engine itself does not.
            return Some(match i32::try_from(n) {
                Ok(small) => CannedResponse::Rows {
                    columns: vec![ColumnSchema::new("?column?", DataType::Int, false)],
                    rows: vec![Row::new(vec![Value::Int(small)])],
                },
                Err(_) => CannedResponse::Rows {
                    columns: vec![ColumnSchema::new("?column?", DataType::BigInt, false)],
                    rows: vec![Row::new(vec![Value::BigInt(n)])],
                },
            });
        }
        if ci_eq(rest_trim, b"null") {
            return Some(CannedResponse::Rows {
                columns: vec![ColumnSchema::new("?column?", DataType::Text, true)],
                rows: vec![Row::new(vec![Value::Null])],
            });
        }
    }
    // v7.39 — the version() canned response ("spg 4.6") is gone: it
    // predated the engine's PG-compatible version() ("PostgreSQL 18.4
    // (SPG-compat)"), and its starts_with match hijacked compound
    // selects like `SELECT version(), now()`.
    // v7.39 (read01 round 118, B3) — `SHOW transaction_isolation` is NO LONGER
    // canned here: a hard-coded "read committed" shadowed the engine handler,
    // so `BEGIN ISOLATION LEVEL REPEATABLE READ; SHOW transaction_isolation`
    // wrongly reported "read committed". Let it fall through to the engine,
    // which reads the live `current_isolation_level`.
    // v7.39 (round 320, V53) — `SHOW search_path`, `SHOW
    // standard_conforming_strings` and `SELECT current_schema()` are NO
    // LONGER canned, for the reason round 118 un-canned `SHOW
    // transaction_isolation` and round 319 un-canned `SELECT current_user`:
    // each answered a fixed value that ignored the client's own `SET`.
    // Measured on PG 18.4: `SET search_path TO app` makes both
    // `SHOW search_path` and `current_schema()` report `app`, and
    // `SET standard_conforming_strings = off` is visible in SHOW. The
    // engine tracks all three.
    // v7.39 (round 319, V52) — `SELECT current_user` is NO LONGER canned
    // here, for the same reason round 118 un-canned `SHOW
    // transaction_isolation`: a hardcoded "admin" shadowed the engine
    // handler, so it reported neither the identity the client connected as
    // nor the effect of a `SET ROLE`. The engine resolves both.
    // v7.39 (round 320, V53) — DISCARD is NO LONGER canned. pgbouncer
    // issues it between pooled client sessions to wipe per-connection
    // state, and the claim that SPG "doesn't have per-connection settings
    // worth wiping" stopped being true when round 279 gave every
    // connection its own session bag — so one pooled client's GUCs and
    // prepared statements survived into the next client's session. The
    // engine implements it. (The old tag for the non-ALL forms was wrong
    // too: PG answers `DISCARD PLANS` / `DISCARD TEMP` / `DISCARD
    // SEQUENCES`, not a bare `DISCARD`.)
    // v4.18: CLUSTER / REINDEX — BI clients (Metabase, DBeaver) run
    // these defensively after schema changes. SPG's parser accepts and
    // ignores both, so the canned tag matches what the engine would do;
    // it is here only to name the command correctly.
    //
    // v7.39 (round 320, V53) — VACUUM and ANALYZE are NOT here any more.
    // Both do real work in the engine (VACUUM reclaims tombstoned
    // versions since round 169, under the in-place MVCC gate; ANALYZE
    // refreshes the planner's statistics), and this short-circuit meant
    // no client reaching SPG over the wire ever got either — the round
    // 169 fix for "a customer's manual reclaim is silently ignored"
    // never actually shipped past pgwire.
    if ci_starts_with(b, b"cluster") {
        return Some(CannedResponse::Tag("CLUSTER"));
    }
    if ci_starts_with(b, b"reindex") {
        return Some(CannedResponse::Tag("REINDEX"));
    }
    // BEGIN ISOLATION LEVEL READ COMMITTED / SERIALIZABLE etc. —
    // pgbouncer + ORMs often prefix transactions with a level.
    // SPG only has one isolation level; accept the syntactic
    // variants without disturbing the engine. Real BEGIN dispatch
    // happens through the normal engine path when it's a bare
    // BEGIN / START TRANSACTION (no isolation specifier).
    // v7.39 (round 320, V53) — `SET TRANSACTION …` is NO LONGER a no-op
    // tag here. It used to be dismissed as "purely informational", which
    // stopped being true once the engine grew real isolation levels: a
    // client asking for SERIALIZABLE was answered `SET` and left on READ
    // COMMITTED. The engine applies the level (and raises PG's 25001 when
    // the transaction has already run a query). BEGIN-ish variants always
    // fell through to the engine and still do.
    // v7.39 — the v4.6 canned pg_catalog subset (pg_class /
    // pg_namespace / pg_database / pg_user / pg_roles / pg_tables) is
    // GONE: it hijacked any SQL that merely mentioned those names and
    // returned a fixed whole table, silently ignoring projections,
    // WHERE clauses and JOINs (found when `SELECT oid, nspname FROM
    // pg_namespace` came back with three columns). The engine has
    // synthesised all six as real meta views for a while, so the
    // regular parse/execute path now serves them with full query
    // semantics.
    None
}

fn ci_contains(b: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() {
        return true;
    }
    if b.len() < needle.len() {
        return false;
    }
    let n = needle.len();
    for i in 0..=b.len() - n {
        if b[i..i + n].eq_ignore_ascii_case(needle) {
            return true;
        }
    }
    false
}

/// v7.37.42-arena Phase 5 fallback — single-pass scan for sequence-
/// mutating function tokens (`setval`/`nextval`/`currval`) that
/// disqualify a SELECT from the read-only streaming wire path
/// (sequence mutation must hit the engine write lock). Anchored on
/// `s`/`n`/`c` to early-skip non-candidates.
fn sql_has_sequence_mutator(b: &[u8]) -> bool {
    if b.len() < 7 {
        return false;
    }
    // v7.39 (GUC) — set_config writes the session GUC store; like the
    // sequence mutators it must reach the engine write lock so
    // `try_exec_set_config` can fire.
    let needles: &[&[u8]] = &[b"setval(", b"nextval(", b"currval(", b"set_config("];
    for i in 0..b.len() {
        let c = b[i] | 0x20;
        match c {
            b's' | b'n' | b'c' => {
                for needle in needles {
                    let n = needle.len();
                    if i + n <= b.len() && b[i..i + n].eq_ignore_ascii_case(needle) {
                        return true;
                    }
                }
            }
            _ => {}
        }
    }
    false
}

/// v7.37.42-arena Phase 5 fallback — single-pass scan for any of the
/// clock-function tokens that disqualify a SELECT from the per-thread
/// parse cache (cached AST has a baked-in clock value that would go
/// stale across calls). The six original needles all share the same
/// short ASCII anchor characters; one O(n) pass over `sql_b` checks
/// for every anchor and confirms the full needle at match candidates,
/// short-circuiting on the first hit. Replaces 6 × ci_contains
/// independent passes.
fn sql_has_clock_function(b: &[u8]) -> bool {
    // Anchor: first character of each needle is `c`, `t`, or `n`
    // (`current_*`, `clock_*`, `transaction_*`, `now(`). Walk once,
    // gate on that anchor, then confirm prefix via eq_ignore_ascii_case.
    if b.len() < 4 {
        return false;
    }
    let needles: &[&[u8]] = &[
        b"current_timestamp",
        b"current_time",
        b"current_date",
        b"clock_timestamp",
        b"transaction_timestamp",
        b"now(",
    ];
    for i in 0..b.len() {
        let c = b[i] | 0x20; // ASCII to lowercase
        match c {
            b'c' | b't' | b'n' => {
                for needle in needles {
                    let n = needle.len();
                    if i + n <= b.len() && b[i..i + n].eq_ignore_ascii_case(needle) {
                        return true;
                    }
                }
            }
            _ => {}
        }
    }
    false
}

enum CannedResponse {
    Rows {
        columns: Vec<ColumnSchema>,
        rows: Vec<Row<'static>>,
    },
    /// v4.15: empty-result statement that just needs a
    /// CommandComplete with a specific tag. Used for DISCARD ALL /
    /// RESET ALL / SET (no-op forms) — pgbouncer sends these
    /// between pooled client sessions.
    Tag(&'static str),
}

impl CannedResponse {
    fn single_text(col: &'static str, val: &'static str) -> Self {
        Self::Rows {
            columns: vec![ColumnSchema::new(col, DataType::Text, false)],
            rows: vec![Row::new(vec![Value::text(val)])],
        }
    }
}

fn send_canned(stream: &mut dyn Write, c: &CannedResponse) -> std::io::Result<()> {
    match c {
        CannedResponse::Rows { columns, rows } => {
            send_row_description(stream, columns)?;
            for row in rows {
                send_data_row(stream, columns, row)?;
            }
            send_command_complete(stream, &format!("SELECT {}", rows.len()))?;
        }
        CannedResponse::Tag(tag) => {
            send_command_complete(stream, tag)?;
        }
    }
    Ok(())
}

// ---- v4.6 pg_catalog synthesizers ----
//
// These return *all* canonical columns from the current SPG catalog
// state — the client filters via WHERE/projection on its side. SPG's
// SQL doesn't support CASE WHEN / regex / function-call selects that
// psql's `\dt` SQL relies on, so psql `\dt` still won't work end-to-end
// without further engine work. Simpler PG drivers and DBeaver-style
// browser panels that issue plain `SELECT ... FROM pg_catalog.X`
// queries do work.

// v6.1.1 — the Parse → Bind → Execute path now runs the engine's
// real prepared-statement API: `Engine::prepare(sql)` parses the
// SQL ONCE into a `Statement` AST (with clock rewrites + ORDER BY
// position resolution applied), and `Engine::execute_prepared(ast,
// params)` substitutes `$N` placeholders inline before dispatch.
// The pre-v6.1.1 implementation re-parsed the SQL on every Execute
// after textual substitution of placeholders — a hack that gave
// pgwire prepared-statement support its on-the-wire shape but
// missed the actual perf win the extended-query protocol exists to
// deliver.

#[derive(Debug, Clone)]
struct PreparedStmt {
    /// Pre-parsed AST. `Engine::prepare` returns this; we clone it
    /// per Execute (cheap — `Statement` is mostly small owned
    /// String / Vec fields).
    ast: spg_sql::ast::Statement,
    /// Number of `$N` placeholders the parsed AST contains. Cached
    /// here so `Bind` can validate the client's parameter count
    /// before constructing the portal.
    placeholder_count: u16,
    /// v6.3.4 — the client-declared OID for each parameter in the
    /// Parse message. `0` means "not declared — Bind format must be
    /// text". Used to dispatch binary-format Bind values to the
    /// right decoder.
    param_type_oids: Vec<u32>,
    /// v7.37 (SPGS small-query bar) — the wire-encoded
    /// `RowDescription` body bytes (NOT including the `b'T'` type
    /// byte + length prefix). Computed at Parse time from the
    /// describe-prepared column list and reused verbatim on every
    /// Execute. Skips per-Execute column reconstruction +
    /// `pg_type_oid` / `pg_type_len` dispatch + Vec growth.
    /// `None` when the prepared statement isn't a SELECT (or
    /// describe_prepared couldn't infer columns).
    row_desc_body: Option<Vec<u8>>,
    /// v7.39 (round 198) — the Parse message's original SQL text.
    /// For a parameterless statement the bind-final SQL IS this
    /// text, so the commit-queue route reuses it instead of deep-
    /// cloning the AST + re-rendering it (~2 ms on a 1000-row
    /// VALUES batch — the r198 zoom's extended-vs-simple gap).
    sql: String,
}

#[derive(Debug, Clone)]
struct Portal {
    /// Reference back to the prepared statement by name. Execute
    /// looks up the AST through `prepared[stmt_name]` rather than
    /// cloning the AST into every portal — most portals are
    /// short-lived (one Execute and then Sync).
    stmt_name: String,
    /// Bound parameter values, already decoded from text format
    /// into typed `spg_storage::Value`s. Empty when the prepared
    /// statement has no `$N` placeholders.
    params: Vec<spg_storage::Value<'static>>,
    /// v7.39 (cursors) — a partially-consumed result set left by an
    /// Execute with max_rows: the next Execute on this portal
    /// resumes here instead of re-running the statement (JDBC
    /// setFetchSize / PortalSuspended protocol).
    suspended: Option<SuspendedRows>,
    /// v7.39 (binary results) — the Bind message's result-format
    /// codes: empty = all text, one entry = applies to every column,
    /// N entries = per column (0 text / 1 binary).
    result_formats: Vec<i16>,
}

/// v7.39 (binary results) — is result column `i` binary under the
/// Bind's format-code list?
fn col_is_binary(formats: &[i16], i: usize) -> bool {
    match formats.len() {
        0 => false,
        1 => formats[0] == 1,
        _ => formats.get(i).copied().unwrap_or(0) == 1,
    }
}

/// v7.39 (cursors) — the materialised remainder of a suspended
/// portal (RowDescription was already sent on the first Execute).
#[derive(Debug, Clone)]
struct SuspendedRows {
    columns: Vec<ColumnSchema>,
    rows: Vec<Row<'static>>,
    cursor: usize,
    formats: Vec<i16>,
    /// v7.39 (round 131) — the CommandComplete tag to emit once the portal is
    /// exhausted, computed from the statement kind at suspend time (a RETURNING
    /// DML keeps its own tag, not `SELECT n`).
    tag: String,
}

/// Parse a null-terminated C string starting at `pos` of `body`.
/// Returns the string + 1-past-null offset for chained reads. Bumps
/// position via outer mutation in callers that read multiple fields.
fn cstring_at(body: &[u8], pos: usize) -> Option<String> {
    let null_off = body[pos..].iter().position(|&b| b == 0)?;
    let bytes = &body[pos..pos + null_off];
    std::str::from_utf8(bytes).ok().map(str::to_string)
}

fn read_cstring<'a>(body: &'a [u8], cursor: &mut usize) -> Option<&'a str> {
    let null_off = body[*cursor..].iter().position(|&b| b == 0)?;
    let bytes = &body[*cursor..*cursor + null_off];
    *cursor += null_off + 1;
    std::str::from_utf8(bytes).ok()
}

fn handle_parse(
    body: &[u8],
    prepared: &mut std::collections::HashMap<String, PreparedStmt>,
    state: &Arc<ServerState>,
) -> Result<(), String> {
    let mut cur = 0;
    let name = read_cstring(body, &mut cur)
        .ok_or("Parse: name not null-terminated UTF-8")?
        .to_string();
    let sql = read_cstring(body, &mut cur)
        .ok_or("Parse: SQL not null-terminated UTF-8")?
        .trim_end_matches(';')
        .trim()
        .to_string();
    // Trailing u16 = param-type count, then that many u32 OIDs.
    // v6.3.4 — these are stored on PreparedStmt so binary-format
    // Bind parameters can be decoded by the right type's wire
    // format.
    if cur + 2 > body.len() {
        return Err("Parse: missing parameter type count".into());
    }
    let oid_count = u16::from_be_bytes([body[cur], body[cur + 1]]) as usize;
    cur += 2;
    if cur + oid_count * 4 > body.len() {
        return Err("Parse: truncated parameter OIDs".into());
    }
    let mut param_type_oids: Vec<u32> = Vec::with_capacity(oid_count);
    for _ in 0..oid_count {
        let oid = u32::from_be_bytes([body[cur], body[cur + 1], body[cur + 2], body[cur + 3]]);
        param_type_oids.push(oid);
        cur += 4;
    }
    let _ = cur; // silence "unused" if we add fields later
    // v6.1.1: real Engine::prepare path — parse + clock-rewrite +
    // ORDER-BY position resolution once, here. Bind/Execute below
    // reuse the AST. Surfaces parser errors as a wire-level Parse
    // failure instead of deferring to the first Execute.
    //
    // v6.3.0: routes through `prepare_cached` so repeat Parse for
    // the same SQL across sessions hits the engine-wide plan cache
    // and skips re-parse + JOIN reorder. Needs `write()` because the
    // cache's LRU promote is `&mut`.
    let mut eng = state
        .engine
        .write()
        .map_err(|_| "Parse: engine lock poisoned".to_string())?;
    let ast = eng
        .prepare_cached(&sql)
        .map_err(|e| format!("Parse: {e}"))?;
    // v7.37 (SPGS small-query bar) — describe at Parse time and
    // cache the wire-format RowDescription body. For repeated
    // executions of the same prepared statement (the sqlx hot
    // shape) every Execute now skips re-deriving the column shape
    // and re-encoding the protocol body.
    let (inferred_oids, columns) = eng.describe_prepared(&ast);
    drop(eng);
    let row_desc_body: Option<Vec<u8>> = if columns.is_empty() {
        None
    } else {
        Some(encode_row_description_body(&columns))
    };
    let placeholder_count = count_placeholders(&sql);
    // v7.39 (binary results) — clients like tokio-postgres declare no
    // param OIDs in Parse and rely on Describe's inference; fill the
    // undeclared slots from the same inference so binary Bind values
    // dispatch to the right decoder.
    let mut param_type_oids = param_type_oids;
    if param_type_oids.len() < inferred_oids.len() {
        param_type_oids.resize(inferred_oids.len(), 0);
    }
    for (slot, inferred) in param_type_oids.iter_mut().zip(inferred_oids.iter()) {
        if *slot == 0 {
            *slot = *inferred;
        }
    }
    prepared.insert(
        name,
        PreparedStmt {
            ast,
            placeholder_count,
            param_type_oids,
            row_desc_body,
            sql,
        },
    );
    Ok(())
}

/// Count distinct `$N` placeholders by scanning. PG numbers them
/// 1..=N; we just want the max N.
fn count_placeholders(sql: &str) -> u16 {
    let bytes = sql.as_bytes();
    let mut max: u32 = 0;
    let mut i = 0;
    while i + 1 < bytes.len() {
        if bytes[i] == b'$' && bytes[i + 1].is_ascii_digit() {
            let mut j = i + 1;
            let mut n: u32 = 0;
            while j < bytes.len() && bytes[j].is_ascii_digit() {
                n = n * 10 + u32::from(bytes[j] - b'0');
                j += 1;
            }
            if n > max {
                max = n;
            }
            i = j;
        } else {
            i += 1;
        }
    }
    u16::try_from(max).unwrap_or(u16::MAX)
}

fn handle_bind(
    body: &[u8],
    prepared: &std::collections::HashMap<String, PreparedStmt>,
) -> Result<(String, Portal), String> {
    let mut cur = 0;
    let portal_name = read_cstring(body, &mut cur)
        .ok_or("Bind: portal name not UTF-8")?
        .to_string();
    let stmt_name = read_cstring(body, &mut cur)
        .ok_or("Bind: statement name not UTF-8")?
        .to_string();
    let stmt = prepared
        .get(&stmt_name)
        .ok_or_else(|| format!("Bind: prepared statement {stmt_name:?} not found"))?;
    // Param-format-codes count (u16), then that many u16 codes:
    // 0 = text, 1 = binary. We only support text for v6.1.1.
    if cur + 2 > body.len() {
        return Err("Bind: truncated format-code count".into());
    }
    let fmt_count = u16::from_be_bytes([body[cur], body[cur + 1]]) as usize;
    cur += 2;
    if cur + fmt_count * 2 > body.len() {
        return Err("Bind: truncated format codes".into());
    }
    let mut formats = Vec::with_capacity(fmt_count);
    for _ in 0..fmt_count {
        formats.push(u16::from_be_bytes([body[cur], body[cur + 1]]));
        cur += 2;
    }
    if cur + 2 > body.len() {
        return Err("Bind: truncated parameter count".into());
    }
    let param_count = u16::from_be_bytes([body[cur], body[cur + 1]]) as usize;
    cur += 2;
    if usize::from(stmt.placeholder_count) != param_count {
        return Err(format!(
            "Bind: parameter count mismatch (SQL has {}, Bind has {param_count})",
            stmt.placeholder_count
        ));
    }
    // v6.1.1: decode text params into typed `Value`s on the spot.
    // SQL NULL → `Value::Null`. Anything numeric-looking → the
    // narrowest fitting numeric variant (Int / BigInt / Float).
    // Boolean tokens land as `Value::Bool`. Everything else stays
    // `Value::Text`; the engine's `coerce_value` path turns Text
    // into the column's declared type at row-insert time, same as
    // simple-query INSERT VALUES would.
    let mut params: Vec<spg_storage::Value<'static>> = Vec::with_capacity(param_count);
    for i in 0..param_count {
        if cur + 4 > body.len() {
            return Err("Bind: truncated parameter length".into());
        }
        let len = i32::from_be_bytes([body[cur], body[cur + 1], body[cur + 2], body[cur + 3]]);
        cur += 4;
        if len < 0 {
            params.push(spg_storage::Value::Null);
            continue;
        }
        let len = len as usize;
        if cur + len > body.len() {
            return Err("Bind: parameter value truncated".into());
        }
        let fmt = match formats.len() {
            0 => 0,
            1 => formats[0],
            _ => formats.get(i).copied().unwrap_or(0),
        };
        if fmt == 1 {
            // v6.3.4 — binary format.
            let oid = stmt.param_type_oids.get(i).copied().unwrap_or(0);
            let v = decode_binary_param(oid, &body[cur..cur + len])?;
            params.push(v);
            cur += len;
            continue;
        }
        if fmt != 0 {
            return Err(format!("Bind: unsupported parameter format code {fmt}"));
        }
        let s = std::str::from_utf8(&body[cur..cur + len])
            .map_err(|_| "Bind: text parameter not valid UTF-8".to_string())?;
        params.push(text_param_to_value(s));
        cur += len;
    }
    // v7.39 (binary results) — trailing result-format codes: count
    // (u16) then that many i16 codes. Honoured per column at Execute.
    let mut result_formats: Vec<i16> = Vec::new();
    if cur + 2 <= body.len() {
        let rf_count = u16::from_be_bytes([body[cur], body[cur + 1]]) as usize;
        cur += 2;
        if cur + rf_count * 2 <= body.len() {
            for _ in 0..rf_count {
                result_formats.push(i16::from_be_bytes([body[cur], body[cur + 1]]));
                cur += 2;
            }
        }
    }
    Ok((
        portal_name,
        Portal {
            stmt_name,
            params,
            suspended: None,
            result_formats,
        },
    ))
}

/// v6.1.1 — convert a pgwire text-format bind parameter into a
/// typed `Value`. Numeric / boolean tokens narrow to the matching
/// scalar so the engine sees a `Literal::Integer(123)` rather
/// than `Literal::String("123")` after the substitute walk (which
/// would then fail to compare against an INT column without an
/// explicit cast). The narrowing is conservative: only inputs
/// that round-trip cleanly to text get the typed treatment.
fn text_param_to_value(s: &str) -> spg_storage::Value<'static> {
    let trimmed = s.trim();
    if trimmed.eq_ignore_ascii_case("true") {
        return spg_storage::Value::Bool(true);
    }
    if trimmed.eq_ignore_ascii_case("false") {
        return spg_storage::Value::Bool(false);
    }
    if let Ok(n) = trimmed.parse::<i32>() {
        return spg_storage::Value::Int(n);
    }
    if let Ok(n) = trimmed.parse::<i64>() {
        return spg_storage::Value::BigInt(n);
    }
    if let Ok(x) = trimmed.parse::<f64>() {
        return spg_storage::Value::Float(x);
    }
    // v6.1.1: PG-vector text format `[f1,f2,...]` — matches pgvector's
    // wire-text representation. A real Vector param avoids parsing
    // the 128-float text literal through the SQL lexer when the same
    // prepared statement runs across many embeddings.
    if let Some(v) = parse_vector_text(trimmed) {
        return spg_storage::Value::vector(v);
    }
    spg_storage::Value::text(s)
}

/// v6.3.4 — decode a binary-format Bind parameter according to its
/// PG type OID. Returns an `EngineError`-shaped string on
/// type/length mismatch so the wire layer can lift it into a Bind
/// error.
///
/// Supported OIDs (matches `pg_type.oid` in stock Postgres):
///   16   = bool          (1 byte: 0/1)
///   17   = bytea         (raw bytes)
///   20   = int8 / bigint (8 bytes BE)
///   21   = int2          (2 bytes BE → SmallInt)
///   23   = int4 / int    (4 bytes BE)
///   25   = text          (UTF-8)
///   700  = float4 / real (4 bytes BE float)
///   701  = float8 / double precision (8 bytes BE float)
///   1043 = varchar       (UTF-8)
///   1082 = date          (4 bytes BE; days since 2000-01-01)
///   1114 = timestamp     (8 bytes BE; microseconds since 2000-01-01 UTC)
///   1184 = timestamptz   (same wire as 1114; UTC)
///   1700 = numeric       (variable-precision packed-digit format)
///
/// Unknown OID + binary format → error (text is the safe default).
fn decode_binary_param(oid: u32, bytes: &[u8]) -> Result<spg_storage::Value<'static>, String> {
    use spg_storage::Value;
    match oid {
        16 => {
            if bytes.len() != 1 {
                return Err(format!(
                    "Bind binary BOOL must be 1 byte, got {}",
                    bytes.len()
                ));
            }
            Ok(Value::Bool(bytes[0] != 0))
        }
        17 | 25 | 1043 => {
            // bytea / text / varchar — for SPG's value space, raw
            // bytes stored as UTF-8 Text. Real BYTEA support is a
            // separate column type; v6.3.4 maps bytea wire bytes into
            // Text via lossless escape (matches PG's text-format
            // bytea = '\\x...' on read).
            if oid == 17 {
                let s =
                    bytes
                        .iter()
                        .fold(String::with_capacity(2 + bytes.len() * 2), |mut acc, b| {
                            if acc.is_empty() {
                                acc.push('\\');
                                acc.push('x');
                            }
                            acc.push_str(&format!("{b:02x}"));
                            acc
                        });
                Ok(Value::text(if s.is_empty() {
                    "\\x".to_string()
                } else {
                    s
                }))
            } else {
                let s = std::str::from_utf8(bytes)
                    .map_err(|_| "Bind binary TEXT/VARCHAR: invalid UTF-8".to_string())?;
                Ok(Value::text(s))
            }
        }
        20 => {
            if bytes.len() != 8 {
                return Err(format!(
                    "Bind binary BIGINT must be 8 bytes, got {}",
                    bytes.len()
                ));
            }
            let n = i64::from_be_bytes(bytes.try_into().unwrap());
            Ok(Value::BigInt(n))
        }
        21 => {
            if bytes.len() != 2 {
                return Err(format!(
                    "Bind binary INT2 must be 2 bytes, got {}",
                    bytes.len()
                ));
            }
            let n = i16::from_be_bytes(bytes.try_into().unwrap());
            Ok(Value::SmallInt(n))
        }
        23 => {
            if bytes.len() != 4 {
                return Err(format!(
                    "Bind binary INT must be 4 bytes, got {}",
                    bytes.len()
                ));
            }
            let n = i32::from_be_bytes(bytes.try_into().unwrap());
            Ok(Value::Int(n))
        }
        700 => {
            if bytes.len() != 4 {
                return Err(format!(
                    "Bind binary REAL must be 4 bytes, got {}",
                    bytes.len()
                ));
            }
            let f = f32::from_be_bytes(bytes.try_into().unwrap()) as f64;
            Ok(Value::Float(f))
        }
        701 => {
            if bytes.len() != 8 {
                return Err(format!(
                    "Bind binary DOUBLE must be 8 bytes, got {}",
                    bytes.len()
                ));
            }
            let f = f64::from_be_bytes(bytes.try_into().unwrap());
            Ok(Value::Float(f))
        }
        1082 => {
            if bytes.len() != 4 {
                return Err(format!(
                    "Bind binary DATE must be 4 bytes, got {}",
                    bytes.len()
                ));
            }
            // Days since 2000-01-01. SPG's Date stores days since
            // 1970-01-01 (Unix epoch), so add the 30-year offset.
            const PG_EPOCH_DAYS_FROM_UNIX: i32 = 10957;
            let pg_days = i32::from_be_bytes(bytes.try_into().unwrap());
            Ok(Value::Date(pg_days + PG_EPOCH_DAYS_FROM_UNIX))
        }
        1114 | 1184 => {
            if bytes.len() != 8 {
                return Err(format!(
                    "Bind binary TIMESTAMP must be 8 bytes, got {}",
                    bytes.len()
                ));
            }
            // Microseconds since 2000-01-01 UTC. SPG stores
            // microseconds since Unix epoch — add the 30-year offset.
            const PG_EPOCH_MICROS_FROM_UNIX: i64 = 946_684_800_000_000;
            let pg_micros = i64::from_be_bytes(bytes.try_into().unwrap());
            Ok(Value::Timestamp(pg_micros + PG_EPOCH_MICROS_FROM_UNIX))
        }
        1700 => decode_binary_numeric(bytes),
        1186 => {
            // v7.37.5 β-P3 — PG `interval` binary format is a fixed
            // 16-byte payload: i64 microseconds (signed, BE) +
            // i32 days (BE) + i32 months (BE). sqlx-postgres,
            // pgx and the typed driver surfaces all send INTERVAL
            // parameters in this shape with format=1 by default.
            // SPG's internal codec stores the same field order
            // little-endian (catalog tag 34); only the byte order
            // differs across the wire/disk boundary.
            if bytes.len() != 16 {
                return Err(format!(
                    "Bind binary INTERVAL must be 16 bytes, got {}",
                    bytes.len()
                ));
            }
            let micros = i64::from_be_bytes(bytes[0..8].try_into().unwrap());
            let days = i32::from_be_bytes(bytes[8..12].try_into().unwrap());
            let months = i32::from_be_bytes(bytes[12..16].try_into().unwrap());
            Ok(Value::Interval {
                months,
                days,
                micros,
            })
        }
        2950 => {
            // v7.37.5 — PG `uuid` binary format is the raw 16-byte
            // RFC 4122 value (network byte order is irrelevant for
            // an opaque 128-bit identifier; PG ships the bytes as
            // stored). sqlx-postgres and other typed drivers send
            // UUID parameters in this format by default.
            if bytes.len() != 16 {
                return Err(format!(
                    "Bind binary UUID must be 16 bytes, got {}",
                    bytes.len()
                ));
            }
            let mut b = [0u8; 16];
            b.copy_from_slice(bytes);
            Ok(Value::Uuid(b))
        }
        0 => Err(
            "Bind: binary format requires the parameter OID to be declared in Parse \
             (got OID=0 meaning unknown)"
                .into(),
        ),
        _ => Err(format!(
            "Bind: binary format for OID {oid} not supported in v6.3.4"
        )),
    }
}

/// PG binary NUMERIC: `i16 ndigits; i16 weight; i16 sign; i16 dscale;
/// i16 digits[ndigits]` (each digit is a base-10000 chunk). Reconstruct
/// to canonical scaled-i128 form.
fn decode_binary_numeric(bytes: &[u8]) -> Result<spg_storage::Value<'static>, String> {
    if bytes.len() < 8 {
        return Err("Bind binary NUMERIC: header truncated".into());
    }
    let ndigits = u16::from_be_bytes([bytes[0], bytes[1]]) as usize;
    let weight = i16::from_be_bytes([bytes[2], bytes[3]]);
    let sign = u16::from_be_bytes([bytes[4], bytes[5]]);
    let dscale = u16::from_be_bytes([bytes[6], bytes[7]]);
    if bytes.len() != 8 + ndigits * 2 {
        return Err(format!(
            "Bind binary NUMERIC: declared ndigits={ndigits} but body has {} bytes",
            bytes.len()
        ));
    }
    if sign == 0xC000 {
        return Err("Bind binary NUMERIC: NaN sign not supported".into());
    }
    let mut digits: Vec<u16> = Vec::with_capacity(ndigits);
    for i in 0..ndigits {
        let off = 8 + i * 2;
        let d = u16::from_be_bytes([bytes[off], bytes[off + 1]]);
        digits.push(d);
    }
    // Build the integer value: sum digit[k] * 10000^(weight - k).
    // Then rescale to `dscale` fractional digits.
    let mut unscaled: i128 = 0;
    let total_digits_after_weight = ndigits as i32 - 1 - weight as i32;
    // exponent shift for each base-10000 digit
    for (k, d) in digits.iter().enumerate() {
        let exp = (weight as i32 - k as i32) * 4;
        let final_exp = exp + dscale as i32;
        if final_exp >= 0 {
            let pow = 10i128.pow(final_exp as u32);
            unscaled = unscaled
                .checked_add((*d as i128).checked_mul(pow).ok_or("NUMERIC overflow")?)
                .ok_or("NUMERIC overflow")?;
        } else {
            let shift = (-final_exp) as u32;
            let pow = 10i128.pow(shift);
            unscaled = unscaled
                .checked_add((*d as i128) / pow)
                .ok_or("NUMERIC overflow")?;
        }
    }
    let _ = total_digits_after_weight; // diagnostic-only
    let final_value = if sign == 0x4000 { -unscaled } else { unscaled };
    // v7.39 (round 271) — dscale is a u16 on the wire and now in the
    // value too, so it no longer has to fit a byte.
    let scale = dscale;
    Ok(spg_storage::Value::Numeric {
        scaled: final_value,
        scale,
        kind: spg_storage::NumericKind::Finite,
    })
}

/// Parse `[f1,f2,...,fn]` into `Vec<f32>`. Returns None on any
/// shape mismatch (no brackets, malformed float, trailing junk) —
/// caller falls back to `Value::Text` so non-vector strings
/// containing `[` still round-trip.
fn parse_vector_text(s: &str) -> Option<Vec<f32>> {
    let bytes = s.as_bytes();
    if bytes.len() < 2 || bytes[0] != b'[' || bytes[bytes.len() - 1] != b']' {
        return None;
    }
    let inner = &s[1..s.len() - 1];
    if inner.trim().is_empty() {
        return Some(Vec::new());
    }
    let mut out = Vec::with_capacity(inner.split(',').count());
    for tok in inner.split(',') {
        let t = tok.trim();
        let f: f32 = t.parse().ok()?;
        if !f.is_finite() {
            return None;
        }
        out.push(f);
    }
    Some(out)
}

// v6.1.1 removed the SQL-textual `substitute_placeholders` helper.
// The Extended Query path now substitutes `Expr::Placeholder(n)`
// nodes into the AST inside the engine — no SQL re-parse per
// Execute. See `Engine::execute_prepared`.

#[allow(clippy::too_many_arguments)]
fn handle_execute(
    body: &[u8],
    portals: &mut std::collections::HashMap<String, Portal>,
    prepared: &std::collections::HashMap<String, PreparedStmt>,
    settings: &std::collections::HashMap<String, String>,
    out: &mut Vec<u8>,
    state: &Arc<ServerState>,
    role: Role,
    tx_state: &mut u8,
    conn_state: &Arc<crate::ConnState>,
) -> Result<(), (&'static str, String)> {
    // v7.37 (SPGS proj_25k bar) — `out` was `&mut dyn Write`
    // pre-7.37; the simple-query Q path already wrote DataRow
    // frames into the response buffer via `encode_data_row`'s
    // single-Vec layout, but the extended-protocol path went
    // through `send_data_row`'s per-row body Vec + `send_msg`
    // body→frame copy. Threading `&mut Vec<u8>` lets the hot
    // SELECT loop below take the same fast path. The handful of
    // upstream calls (`send_row_description`, `send_command_complete`,
    // `send_msg`) already accept `impl Write`, which Vec<u8>
    // satisfies.
    let stream: &mut Vec<u8> = out;
    // v7.17.0 Phase 2.3 — protocol-level errors keep SQLSTATE
    // `42000` to match prior behavior; only `EngineError::Cancelled`
    // promotes to `57014` via `engine_error_to_wire`.
    let proto = |m: String| ("42000", m);
    let mut cur = 0;
    let portal_name = read_cstring(body, &mut cur)
        .ok_or_else(|| proto("Execute: portal name not UTF-8".to_string()))?;
    // v7.39 (cursors) — max-rows (i32, 0 = unlimited). A positive
    // value caps this Execute's DataRows; a capped result leaves the
    // remainder suspended on the portal and answers PortalSuspended.
    if cur + 4 > body.len() {
        return Err(proto("Execute: missing max-rows".to_string()));
    }
    let max_rows =
        u32::from_be_bytes([body[cur], body[cur + 1], body[cur + 2], body[cur + 3]]) as usize;
    let portal_key = portal_name.to_string();
    // Resume a suspended portal: emit the next batch from the cached
    // remainder — the statement does NOT re-run (PG cursor semantics).
    if let Some(p) = portals.get_mut(&portal_key)
        && let Some(susp) = p.suspended.as_mut()
    {
        let stream: &mut Vec<u8> = out;
        let end = if max_rows == 0 {
            susp.rows.len()
        } else {
            (susp.cursor + max_rows).min(susp.rows.len())
        };
        let row_arena = bumpalo::Bump::new();
        let any_binary = susp.formats.iter().any(|&f| f == 1);
        let (wire_style, wire_tz) = state
            .engine
            .read()
            .map(|e| (e.render_style(), e.session_tz()))
            .unwrap_or((Default::default(), spg_engine::SessionTz::Utc));
        for row in &susp.rows[susp.cursor..end] {
            if any_binary {
                encode_data_row_formats(
                    stream,
                    &susp.columns,
                    row,
                    &susp.formats,
                    &row_arena,
                    &wire_style,
                    &wire_tz,
                )
                .map_err(|e| proto(e.to_string()))?;
            } else {
                encode_data_row(
                    stream,
                    &susp.columns,
                    row,
                    &row_arena,
                    &wire_style,
                    &wire_tz,
                )
                .map_err(|e| proto(e.to_string()))?;
            }
        }
        susp.cursor = end;
        if end < susp.rows.len() {
            send_msg(stream, b's', &[]).map_err(|e| proto(e.to_string()))?;
        } else {
            // v7.39 (round 131) — reuse the verb-correct tag captured at suspend
            // time (a RETURNING DML is tagged `INSERT 0 n` / `MERGE n`, not
            // `SELECT n`).
            let tag = susp.tag.clone();
            p.suspended = None;
            send_command_complete(stream, &tag).map_err(|e| proto(e.to_string()))?;
        }
        return Ok(());
    }
    let portal = portals
        .get(portal_name)
        .ok_or_else(|| proto(format!("Execute: portal {portal_name:?} not found")))?;
    let stmt = prepared.get(&portal.stmt_name).ok_or_else(|| {
        proto(format!(
            "Execute: prepared statement {:?} dropped while a portal held a reference",
            portal.stmt_name
        ))
    })?;
    // v7.17.0 Phase 2.3 — resolve per-statement deadline before
    // taking the engine lock so the lock-hold window matches the
    // simple-query path; the cancel token rides into
    // `execute_prepared_with_cancel`.
    // v7.39 (query cancel) — arm this statement with the session's
    // CancelRequest flag (cleared first: an idle-time cancel must not
    // kill the NEXT statement, matching PG).
    conn_state
        .cancel_flag
        .store(false, std::sync::atomic::Ordering::Relaxed);
    let cancel = statement_cancel(settings, &conn_state.cancel_flag);
    // v6.1.1: dispatch through `Engine::execute_prepared` — the
    // AST is reused from Parse; only the substitute walk + dispatch
    // happen here. No SQL re-parse, no canned-response check (the
    // canned probes are simple-query shape only; an ORM that
    // PREPAREs `SELECT version()` doesn't need the canned path
    // because the engine itself will satisfy it).
    let needs_write = !matches!(&stmt.ast, spg_sql::ast::Statement::Select(_));
    // v7.37 (SPGS PROJ wire encode tax) — streaming SELECT path for
    // SELECT prepared statements with no bound parameters. Skips the
    // intermediate `Vec<Row>` and emits DataRow frames straight into
    // `stream` as the engine produces each surviving row. For
    // streamable shapes (joined non-aggregate projection) the engine
    // also skips the per-cell `.cloned()` and emits cell references
    // straight out of the source tables.
    let cached_row_desc = stmt.row_desc_body.clone();
    let wants_binary = portal.result_formats.iter().any(|&f| f == 1);
    if let (spg_sql::ast::Statement::Select(s), true, 0, false) =
        (&stmt.ast, portal.params.is_empty(), max_rows, wants_binary)
    {
        let mut eng = state
            .engine
            .write()
            .map_err(|_| proto("Execute: engine lock poisoned".to_string()))?;
        if matches!(role, Role::ReadOnly) {
            // SELECT is always read-allowed; the role check stays
            // here for symmetry with the non-streaming branch's
            // `needs_write` arm.
            let _ = needs_write;
        }
        let mut cols_storage: Vec<ColumnSchema> = Vec::new();
        let wire_style = eng.render_style();
        let wire_tz = eng.session_tz();
        // v7.37.42-arena Phase 3 — per-Execute arena for cell
        // text-format fallback payloads in the extended-protocol
        // streaming path.
        let ext_arena = bumpalo::Bump::new();
        let stream_emit_result =
            eng.execute_prepared_select_streaming(s, cancel, |item| match item {
                spg_engine::StreamItem::Header(cols) => {
                    // v7.39 — Execute must not emit RowDescription
                    // (Describe owns it); keep the columns for the
                    // row encoder only.
                    cols_storage.extend_from_slice(cols);
                    let _ = &cached_row_desc;
                    Ok(())
                }
                spg_engine::StreamItem::Row(values) => encode_data_row_from_refs(
                    stream,
                    &cols_storage,
                    values,
                    &ext_arena,
                    &wire_style,
                    &wire_tz,
                )
                .map_err(|e| spg_engine::EngineError::Unsupported(e.to_string())),
            });
        drop(eng);
        let row_count = match stream_emit_result {
            Ok(n) => n,
            Err(e) => {
                let (sqlstate, msg) = engine_error_to_wire_conn(&e, conn_state);
                return Err((sqlstate, msg));
            }
        };
        send_command_complete(stream, &format!("SELECT {row_count}"))
            .map_err(|e| proto(e.to_string()))?;
        return Ok(());
    }
    // r178 — plain autocommit DML (no RETURNING) goes through the
    // commit barrier: the leader executes the bind-final SQL and the
    // group shares one fsync with every concurrent writer (native
    // wrap parity). Same exclusions as the simple-query route.
    let plain_dml = {
        use spg_sql::ast::Statement as S;
        match &stmt.ast {
            S::Insert(i) => i.returning.is_none(),
            S::Update(u) => u.returning.is_none(),
            S::Delete(d) => d.returning.is_none(),
            S::Merge(m) => m.returning.is_none(),
            _ => false,
        }
    };
    let timeout_set = settings
        .get("statement_timeout")
        .map(String::as_str)
        .and_then(parse_timeout_ms)
        .unwrap_or(0)
        > 0;
    let (result, queue_persisted) = if *tx_state == b'I'
        && state.wal.is_some()
        && plain_dml
        && !timeout_set
    {
        if matches!(role, Role::ReadOnly) {
            return Err(proto("permission denied: readonly role".to_string()));
        }
        // r198 — a parameterless statement's bind-final SQL IS the
        // Parse text; skip the AST deep-clone + re-render (~2 ms on
        // a 1000-row VALUES batch).
        let bind_sql = if portal.params.is_empty() {
            stmt.sql.clone()
        } else {
            let mut bind_ast = stmt.ast.clone();
            spg_engine::substitute_placeholders(&mut bind_ast, &portal.params)
                .map_err(|e| proto(format!("Execute: bind-final render failed: {e}")))?;
            bind_ast.to_string()
        };
        // Fresh per-task flag — same reasoning as the simple-query
        // route (no watchdog on this path; timeout sessions excluded).
        let queue_flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (result, wal_outcome) =
            crate::commit_queue_execute(state, bind_sql.clone(), &queue_flag);
        if let Err(e) = wal_outcome {
            return Err(proto(format!("Execute: durability append failed: {e}")));
        }
        if matches!(&result, Ok(QueryResult::CommandOk { .. }))
            && state.audit_path.is_some()
            && let Err(e) = crate::append_audit_pub(state, &bind_sql)
        {
            return Err(proto(format!("Execute: audit append failed: {e}")));
        }
        (result, true)
    } else {
        let result = {
            // execute_prepared takes &mut self for symmetry with the
            // simple-query path, so both read and write hold the write
            // lock for the duration (single-writer transactional state).
            let mut eng = state
                .engine
                .write()
                .map_err(|_| proto("Execute: engine lock poisoned".to_string()))?;
            // Role gate — same shape as `execute_with_role`.
            if needs_write && matches!(role, Role::ReadOnly) {
                return Err(proto("permission denied: readonly role".to_string()));
            }
            eng.execute_prepared_with_cancel(stmt.ast.clone(), &portal.params, cancel)
        };
        (result, false)
    };
    // v7.33 (A1) — persist the write to the WAL (or the no-WAL snapshot)
    // BEFORE acking it. Pre-7.33 `handle_execute` persisted nothing, so
    // server-mode prepared writes were lost on crash. Render the
    // bind-final SQL (placeholders substituted to literals — the same
    // walk execute_prepared just ran, so replay reproduces the effect)
    // and route it through the shared persister both pgwire paths use.
    // Queue-routed statements were already appended by the leader.
    // r178 — RETURNING DML answers Rows; persist those too (the
    // shared persister re-checks the statement shape itself).
    if !queue_persisted
        && needs_write
        && matches!(
            &result,
            Ok(QueryResult::CommandOk { .. }) | Ok(QueryResult::Rows { .. })
        )
    {
        let mut bind_ast = stmt.ast.clone();
        spg_engine::substitute_placeholders(&mut bind_ast, &portal.params)
            .map_err(|e| proto(format!("Execute: bind-final render failed: {e}")))?;
        persist_wire_write(state, &bind_ast.to_string(), &result, conn_state.tx_id)
            .map_err(|e| proto(format!("Execute: durability append failed: {e}")))?;
    }
    let (wire_style, wire_tz) = state
        .engine
        .read()
        .map(|e| (e.render_style(), e.session_tz()))
        .unwrap_or((Default::default(), spg_engine::SessionTz::Utc));
    match result {
        Ok(QueryResult::Rows { columns, rows }) => {
            // v7.39 (binary results / tokio-postgres) — PG's Execute
            // response carries NO RowDescription (that belongs to
            // Describe); strict clients treat a 'T' here as an
            // unexpected message. The pre-7.39 behaviour of re-sending
            // it per Execute broke tokio-postgres's query path.
            let n = rows.len();
            // v7.37.42-arena Phase 3 — per-Execute arena for the cell
            // text-format fallback payloads in the materialised
            // extended-protocol path.
            let row_arena = bumpalo::Bump::new();
            // v7.39 (cursors) — cap the batch and suspend the rest.
            let emit_end = if max_rows > 0 { max_rows.min(n) } else { n };
            let formats = portals
                .get(&portal_key)
                .map(|p| p.result_formats.clone())
                .unwrap_or_default();
            for row in &rows[..emit_end] {
                if wants_binary {
                    encode_data_row_formats(
                        stream,
                        &columns,
                        row,
                        &formats,
                        &row_arena,
                        &wire_style,
                        &wire_tz,
                    )
                    .map_err(|e| proto(e.to_string()))?;
                } else {
                    encode_data_row(stream, &columns, row, &row_arena, &wire_style, &wire_tz)
                        .map_err(|e| proto(e.to_string()))?;
                }
            }
            let tag = command_tag_for_rows_ast(&stmt.ast, n);
            if emit_end < n {
                send_msg(stream, b's', &[]).map_err(|e| proto(e.to_string()))?;
                if let Some(p) = portals.get_mut(&portal_key) {
                    p.suspended = Some(SuspendedRows {
                        columns,
                        rows,
                        cursor: emit_end,
                        formats,
                        tag,
                    });
                }
            } else {
                send_command_complete(stream, &tag).map_err(|e| proto(e.to_string()))?;
            }
        }
        Ok(QueryResult::CommandOk { affected, .. }) => {
            // Synthesise a command tag from the statement kind so
            // drivers see e.g. "INSERT 0 1" rather than the
            // simple-query path's per-SQL synthesis. We re-derive
            // the tag from the AST root, not the original SQL
            // text — text is owned by Parse, not Execute.
            let tag = command_tag_for_ast(&stmt.ast, affected);
            send_command_complete(stream, &tag).map_err(|e| proto(e.to_string()))?;
            *tx_state = if state.engine.read().is_ok_and(|e| e.is_tx_open(conn_state.tx_id)) {
                b'T'
            } else {
                b'I'
            };
        }
        Err(e) => {
            let (sqlstate, msg) = engine_error_to_wire_conn(&e, conn_state);
            return Err((sqlstate, msg));
        }
        // v7.5.0 — QueryResult is #[non_exhaustive].
        Ok(_) => return Err(proto("unexpected QueryResult variant".to_string())),
    }
    Ok(())
}

/// v6.1.1 — command-tag lookup that consumes the AST root directly,
/// avoiding the simple-query path's text-based heuristics. PG's
/// "tag" string is what shows up in psql as "INSERT 0 1" / "UPDATE
/// 3" — most drivers parse it, so the shape matters.
/// v7.39 (round 131) — command tag for a Rows result. A data-modifying
/// statement with RETURNING keeps its own tag (`INSERT 0 n` / `UPDATE n` /
/// `DELETE n` / `MERGE n`, `n` = returned rows); everything else that yields
/// rows (SELECT / VALUES / SHOW / a read-only CTE) tags `SELECT n`. PG tags all
/// four RETURNING forms this way — the pre-7.39 `SELECT n` was a divergence
/// shared across INSERT/UPDATE/DELETE/MERGE RETURNING.
fn command_tag_for_rows_ast(stmt: &spg_sql::ast::Statement, n: usize) -> String {
    use spg_sql::ast::Statement;
    match stmt {
        Statement::Insert(_) => format!("INSERT 0 {n}"),
        Statement::Update(_) => format!("UPDATE {n}"),
        Statement::Delete(_) => format!("DELETE {n}"),
        Statement::Merge(_) => format!("MERGE {n}"),
        _ => format!("SELECT {n}"),
    }
}

/// v7.39 (round 131) — `command_tag_for_rows_ast` for the simple-query path,
/// which holds the SQL text rather than the AST. Only a Rows-yielding statement
/// reaches this, so an `INSERT`/`UPDATE`/`DELETE`/`MERGE` first word implies a
/// RETURNING clause. A data-modifying `WITH … RETURNING` is tagged by its
/// top-level statement (recovered by parsing).
fn command_tag_for_rows_sql(sql: &str, n: usize) -> String {
    let first = sql
        .trim_start()
        .split_ascii_whitespace()
        .next()
        .unwrap_or("")
        .to_ascii_uppercase();
    match first.as_str() {
        "INSERT" => format!("INSERT 0 {n}"),
        "UPDATE" => format!("UPDATE {n}"),
        "DELETE" => format!("DELETE {n}"),
        "MERGE" => format!("MERGE {n}"),
        // v7.39 (round 219) — a cursor FETCH tags with its row count.
        "FETCH" => format!("FETCH {n}"),
        "WITH" => spg_sql::parser::parse_statement(sql)
            .map(|s| command_tag_for_rows_ast(&s, n))
            .unwrap_or_else(|_| format!("SELECT {n}")),
        _ => format!("SELECT {n}"),
    }
}

fn command_tag_for_ast(stmt: &spg_sql::ast::Statement, affected: usize) -> String {
    use spg_sql::ast::Statement;
    match stmt {
        Statement::Insert(_) => format!("INSERT 0 {affected}"),
        Statement::Update(_) => format!("UPDATE {affected}"),
        Statement::Delete(_) => format!("DELETE {affected}"),
        // v7.38 (read01 P3.27) — PG tags MERGE with its total touched-row
        // count, no leading OID field.
        Statement::Merge(_) => format!("MERGE {affected}"),
        Statement::CreateTable(_) => "CREATE TABLE".to_string(),
        Statement::DropTable { .. } => "DROP TABLE".to_string(),
        Statement::AlterTable(_) => "ALTER TABLE".to_string(),
        Statement::CreateIndex(_) => "CREATE INDEX".to_string(),
        Statement::AlterIndex(_) => "ALTER INDEX".to_string(),
        Statement::CreateView(_) => "CREATE VIEW".to_string(),
        Statement::DropView { .. } => "DROP VIEW".to_string(),
        Statement::CreateSequence(_) => "CREATE SEQUENCE".to_string(),
        Statement::Truncate { .. } => "TRUNCATE TABLE".to_string(),
        Statement::Begin(_) => "BEGIN".to_string(),
        Statement::Commit => "COMMIT".to_string(),
        Statement::Rollback => "ROLLBACK".to_string(),
        Statement::Savepoint(_) => "SAVEPOINT".to_string(),
        Statement::RollbackToSavepoint(_) => "ROLLBACK".to_string(),
        Statement::ReleaseSavepoint(_) => "RELEASE".to_string(),
        // v7.39 (read01 utils/adt) — PG tags the TYPE family fully.
        Statement::CreateType(_) => "CREATE TYPE".to_string(),
        Statement::AlterTypeAddValue { .. } => "ALTER TYPE".to_string(),
        Statement::DropType { .. } => "DROP TYPE".to_string(),
        // PG tags CREATE USER as CREATE ROLE (USER is the role alias).
        Statement::CreateUser(_) => "CREATE ROLE".to_string(),
        Statement::DropUser { .. } => "DROP ROLE".to_string(),
        // v6.1.2 — PG tag for `CREATE PUBLICATION` / `DROP PUBLICATION`.
        // PG's tag does not include the publication name; we match.
        Statement::CreatePublication(_) => "CREATE PUBLICATION".to_string(),
        Statement::DropPublication(_) => "DROP PUBLICATION".to_string(),
        // v6.1.4 — symmetric for subscriptions.
        Statement::CreateSubscription(_) => "CREATE SUBSCRIPTION".to_string(),
        Statement::DropSubscription(_) => "DROP SUBSCRIPTION".to_string(),
        // Select / Show / Explain go through the Rows path above.
        _ => "OK".to_string(),
    }
}

// ---- v4.19 SET / SHOW session-variable helpers ----

/// Parse `SET name = value` / `SET name TO value` (plus optional
/// `SESSION` or `LOCAL` keyword). Returns the (name, value) pair
/// or None if the SQL isn't a SET we handle.
fn parse_set_statement(sql: &str) -> Option<(String, String)> {
    let trimmed = sql.trim();
    // v7.34.3 — prefix-gate WITHOUT lowercasing the whole SQL. Most
    // queries are not SET statements; bail on the cheap byte-CI check
    // before paying a `to_ascii_lowercase()` String allocation.
    if !ci_starts_with(trimmed.as_bytes(), b"set ") {
        return None;
    }
    let lower = trimmed.to_ascii_lowercase();
    let rest = lower.strip_prefix("set ")?;
    // v7.39 (GUC) — SET LOCAL is tx-scoped and owned by the engine's
    // undo log; the wire cache must not shadow it (a COMMIT/ROLLBACK
    // would leave the stale value visible to SHOW).
    if rest.starts_with("local ") {
        return None;
    }
    // Strip optional SESSION.
    let rest = rest.strip_prefix("session ").unwrap_or(rest);
    // Find name + assign operator (= or TO).
    let (name, value_part) = if let Some(idx) = rest.find('=') {
        (rest[..idx].trim().to_string(), rest[idx + 1..].trim())
    } else {
        let idx = rest.find(" to ")?;
        (rest[..idx].trim().to_string(), rest[idx + 4..].trim())
    };
    if name.is_empty() {
        return None;
    }
    // Strip surrounding quotes from the value.
    let value = value_part.trim_matches('\'').trim_matches('"').to_string();
    Some((name, value))
}

/// Parse `SHOW name` / `SHOW ALL` / `SHOW SESSION AUTHORIZATION`.
/// Returns the requested name, lowercased, or None.
fn parse_show_statement(sql: &str) -> Option<String> {
    let trimmed = sql.trim();
    if !ci_starts_with(trimmed.as_bytes(), b"show ") {
        return None;
    }
    let lower = trimmed.to_ascii_lowercase();
    let rest = lower.strip_prefix("show ")?;
    // PG spells the timezone GUC as two words in SHOW.
    if rest.trim().trim_end_matches(';').trim() == "time zone" {
        return Some("timezone".to_string());
    }
    // v7.39 (read01 round 118, B3) — `SHOW TRANSACTION ISOLATION LEVEL` is PG's
    // multi-word spelling of the `transaction_isolation` GUC; normalise it so
    // the live-value path (`session_param`) serves it, not the first word
    // ("transaction").
    if rest.trim().trim_end_matches(';').trim() == "transaction isolation level" {
        return Some("transaction_isolation".to_string());
    }
    let name = rest.split_ascii_whitespace().next()?.to_string();
    Some(name)
}

/// Render a SHOW result: the value from `settings` first, else a
/// known default. SHOW ALL emits one row per known setting.
fn render_show(name: &str, settings: &std::collections::HashMap<String, String>) -> CannedResponse {
    if name == "all" {
        let mut entries: Vec<(String, String)> = known_defaults()
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        // Overlay session overrides.
        for (k, v) in settings {
            if let Some(pos) = entries.iter().position(|(name, _)| name == k) {
                entries[pos].1.clone_from(v);
            } else {
                entries.push((k.clone(), v.clone()));
            }
        }
        entries.sort();
        let columns = vec![
            ColumnSchema::new("name", DataType::Text, false),
            ColumnSchema::new("setting", DataType::Text, false),
            ColumnSchema::new("description", DataType::Text, true),
        ];
        let rows: Vec<Row<'static>> = entries
            .into_iter()
            .map(|(n, v)| Row::new(vec![Value::text(n), Value::text(v), Value::Null]))
            .collect();
        return CannedResponse::Rows { columns, rows };
    }
    let value = settings
        .get(name)
        .cloned()
        .or_else(|| {
            known_defaults()
                .iter()
                .find(|(k, _)| *k == name)
                .map(|(_, v)| (*v).to_string())
        })
        .unwrap_or_default();
    let columns = vec![ColumnSchema::new(name.to_string(), DataType::Text, false)];
    CannedResponse::Rows {
        columns,
        rows: vec![Row::new(vec![Value::text(value)])],
    }
}

/// Built-in PG GUCs we report sane defaults for so clients
/// configuring themselves at startup don't get an empty SHOW.
fn known_defaults() -> &'static [(&'static str, &'static str)] {
    &[
        ("application_name", ""),
        ("client_encoding", "UTF8"),
        ("datestyle", "ISO, MDY"),
        // v7.39 (read01 round 44) — PG's initdb default for english/UTF-8.
        ("default_text_search_config", "pg_catalog.english"),
        ("default_transaction_isolation", "read committed"),
        ("default_transaction_read_only", "off"),
        // v7.39 (round 204) — memory GUC boot defaults so `SHOW
        // work_mem` on a fresh connection reports PG's value, not
        // empty. Canonicalized human units (the engine stores + SHOWs
        // these forms; see session::render_pg_mem_kb).
        ("work_mem", "4MB"),
        ("maintenance_work_mem", "64MB"),
        ("shared_buffers", "128MB"),
        ("effective_cache_size", "4GB"),
        ("client_min_messages", "notice"),
        ("intervalstyle", "postgres"),
        ("search_path", "\"$user\", public"),
        ("server_encoding", "UTF8"),
        ("server_version", "18.4 (spg-4.19)"),
        ("server_version_num", "180004"),
        ("standard_conforming_strings", "on"),
        ("statement_timeout", "0"),
        ("timezone", "UTC"),
        ("transaction_isolation", "read committed"),
        ("transaction_read_only", "off"),
    ]
}

// ---- v7.17.0 Phase 2.3 — statement_timeout ----

/// Monotonic now in microseconds. Origin = first call into
/// `Instant::now()` on this process; subsequent calls measure
/// elapsed time against that origin. Used to feed
/// `CancelToken::with_deadline`'s `now_fn` slot — the engine is
/// `#![no_std]` and so can't reach `std::time::Instant` itself.
fn monotonic_now_us() -> u64 {
    use std::sync::OnceLock;
    use std::time::Instant;
    static ORIGIN: OnceLock<Instant> = OnceLock::new();
    let origin = ORIGIN.get_or_init(Instant::now);
    let micros = origin.elapsed().as_micros();
    u64::try_from(micros).unwrap_or(u64::MAX)
}

/// v7.37.42-arena Phase 5 fallback — wall-clock UNIX µs derived from
/// a one-shot boot `(Instant, SystemTime)` pair + monotonic delta.
/// `SystemTime::now()` is a vDSO syscall (~30-50 ns on Linux,
/// ~50-150 ns on other kernels) while `Instant::now()` is a pure
/// monotonic clock_gettime (~5-10 ns). For the per-query
/// `last_query_start_us` activity-registry update — which is purely
/// diagnostic and tolerates `Instant`-derived wall-clock drift over
/// the process lifetime — this swap pays back ~20-40 ns / query.
/// Negligible per call, but every wire-probe SCALARSQ visits this
/// path, so the cumulative attack budget bills it as one of the
/// small-wins.
fn wallclock_unix_micros() -> i64 {
    use std::sync::OnceLock;
    use std::time::{Instant, SystemTime, UNIX_EPOCH};
    static ORIGIN: OnceLock<(Instant, i64)> = OnceLock::new();
    let (boot_instant, boot_unix_us) = *ORIGIN.get_or_init(|| {
        let i = Instant::now();
        let u = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_micros() as i64)
            .unwrap_or(0);
        (i, u)
    });
    let elapsed_us = boot_instant.elapsed().as_micros() as i64;
    boot_unix_us.saturating_add(elapsed_us)
}

/// Parse PG `statement_timeout` value into milliseconds. Accepts:
/// bare integer (ms), or `<n>` followed by a unit suffix
/// (`us` / `ms` / `s` / `min` / `h` / `d`). `0` is valid and
/// disables the timeout. Returns `None` on garbage.
fn parse_timeout_ms(s: &str) -> Option<u64> {
    let t = s.trim();
    if t.is_empty() {
        return None;
    }
    // Bare integer = milliseconds (matches PG: `SET ... = 5000`).
    if let Ok(n) = t.parse::<u64>() {
        return Some(n);
    }
    let split_at = t.find(|c: char| c.is_ascii_alphabetic()).unwrap_or(t.len());
    let (num_part, unit) = t.split_at(split_at);
    let num: u64 = num_part.trim().parse().ok()?;
    let mult: u64 = match unit.trim().to_ascii_lowercase().as_str() {
        // Sub-ms granularity floors to 0; PG itself clamps to ≥1ms
        // in practice, so this matches observed behavior closely.
        "us" => return Some(num / 1000),
        "ms" => 1,
        "s" => 1_000,
        "min" | "m" => 60_000,
        "h" => 3_600_000,
        "d" => 86_400_000,
        _ => return None,
    };
    num.checked_mul(mult)
}

/// Build the engine `CancelToken` for the next statement off the
/// pgwire session `settings` map. When `statement_timeout` is `0`,
/// missing, or unparseable, returns `CancelToken::none()` so the
/// hot path (default SLO load) elides the deadline check entirely
/// — costing only one predicted-not-taken branch in
/// `CancelToken::is_cancelled`.
fn statement_cancel<'a>(
    settings: &std::collections::HashMap<String, String>,
    // v7.39 (query cancel) — the session's CancelRequest flag; every
    // statement token carries it (reset by the caller at statement
    // start so an idle-time cancel is a no-op, like PG).
    flag: &'a std::sync::atomic::AtomicBool,
) -> CancelToken<'a> {
    let base = CancelToken::from_flag(flag);
    let raw = settings
        .get("statement_timeout")
        .map(String::as_str)
        .unwrap_or("0");
    let Some(ms) = parse_timeout_ms(raw) else {
        return base;
    };
    if ms == 0 {
        return base;
    }
    let now_fn: MonotonicNowFn = monotonic_now_us;
    let deadline_us = monotonic_now_us().saturating_add(ms.saturating_mul(1_000));
    base.with_deadline(now_fn, deadline_us)
}

/// v7.39 — a per-connection cancel secret. Not cryptographic (PG's
/// is a plain 32-bit value too); mixed from the thread-unique
/// RandomState hasher so parallel connections don't collide.
pub(crate) fn new_cancel_secret() -> u32 {
    use std::hash::{BuildHasher, Hasher};
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u64(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0),
    );
    (h.finish() >> 16) as u32
}

/// Map an `EngineError` to (SQLSTATE, wire message). v7.17.0
/// Phase 2.3 — separates `Cancelled` so it lands as PG's standard
/// `57014` (`query_canceled`) with the canonical "canceling
/// statement due to statement timeout" text driver libraries grep
/// for. Everything else stays on the legacy `42000` to preserve
/// existing client-side error parsing.
/// v7.39 (query cancel) — like [`engine_error_to_wire`], but a
/// `Cancelled` raised while the session's CancelRequest flag is set
/// reports PG's "user request" text instead of the timeout text
/// (drivers grep both forms).
fn engine_error_to_wire_conn(
    e: &EngineError,
    conn_state: &crate::ConnState,
) -> (&'static str, String) {
    if matches!(e, EngineError::Cancelled)
        && conn_state
            .cancel_flag
            .load(std::sync::atomic::Ordering::Relaxed)
    {
        return (
            "57014",
            "canceling statement due to user request".to_string(),
        );
    }
    engine_error_to_wire(e)
}

/// The internal class prefixes `Display for EngineError` (and `EvalError`) add.
/// Longest first: "eval: type mismatch: " must strip before "eval: ".
fn strip_error_class(msg: &str) -> String {
    const CLASSES: &[&str] = &[
        "eval: type mismatch: ",
        "eval: ",
        "unsupported: ",
        // r184 — lexer errors surface as "parse: lex: …"; the combined
        // prefix must strip in one pass (the list strips only the first
        // match) so psql shows PG's exact message shape. Longest first.
        "parse: lex: ",
        "lex: ",
        "parse: ",
        "storage: ",
    ];
    for c in CLASSES {
        if let Some(rest) = msg.strip_prefix(c) {
            return rest.to_string();
        }
    }
    msg.to_string()
}

/// v7.39 (read01 round 95) — recover PG's 1-based error position for a parse
/// failure so the wire can attach the ErrorResponse `P` field. The position is
/// re-derived from the query text on this cold error path (rather than carried
/// on `ParseError`, which would grow the recursive parse stack). Standard PG
/// string mode (`backslash_escapes = false`) is used — it doesn't shift token
/// offsets in practice. Only syntax errors carry a position today; semantic
/// errors (column-not-found, type mismatch) would need analyzer/eval plumbing
/// and are deferred.
/// v7.39 (read01 round 230) — SQLSTATE for the window-clause errors, or
/// `None` when the message isn't one. PG answers every window-clause
/// complaint with 42P20 WINDOWING_ERROR and reserves 42704
/// UNDEFINED_OBJECT for a genuinely missing window name — a split worth
/// keeping straight, since the copy and redefinition wordings also carry
/// `window "w1"`. Matches on substrings because the engine's `Unsupported`
/// Display prefixes the message.
fn window_sqlstate(msg: &str) -> Option<&'static str> {
    if msg.contains("window functions are not allowed in ")
        || msg.contains("frame start cannot be ")
        || msg.contains("frame end cannot be ")
        || msg.contains("frame starting from ")
        || msg.contains("cannot override PARTITION BY clause of window ")
        || msg.contains("cannot override ORDER BY clause of window ")
        || msg.contains("because it has a frame clause")
        || msg.contains("RANGE with offset PRECEDING/FOLLOWING ")
        || (msg.contains("window \"") && msg.contains("\" is already defined"))
    {
        return Some("42P20");
    }
    if msg.contains("window \"") && msg.contains("\" does not exist") {
        return Some("42704");
    }
    // v7.39 (round 230) — PG implements neither modifier for a windowed
    // call and reports the gap as 0A000 FEATURE_NOT_SUPPORTED.
    if msg.contains("is not implemented for window functions") {
        return Some("0A000");
    }
    None
}

fn parse_error_position(e: &EngineError, sql: &str) -> Option<usize> {
    match e {
        EngineError::Parse(pe) => spg_sql::parser::syntax_error_position(sql, false, pe.token_pos),
        _ => None,
    }
}

fn engine_error_to_wire(e: &EngineError) -> (&'static str, String) {
    if let EngineError::Cancelled = e {
        return (
            "57014",
            "canceling statement due to statement timeout".to_string(),
        );
    }
    // v7.38 (read01 P3.26) — an aborted-transaction rejection carries PG's
    // 25P02 so clients recognise "commands ignored until end of block".
    if let EngineError::InFailedTransaction = e {
        return ("25P02", e.to_string());
    }
    // v7.38 (read01 P4.02) — a single-row subquery that returned many rows
    // is PG's 21000 CARDINALITY_VIOLATION.
    if let EngineError::CardinalityViolation = e {
        return ("21000", e.to_string());
    }
    // v7.37.17 (Phase E3) — a RR/SER commit that hit a write-write
    // conflict is PG's 40001 SERIALIZATION_FAILURE (clients retry).
    if let EngineError::SerializationFailure(_) = e {
        return ("40001", e.to_string());
    }
    // v7.39 (read01 round 232) — the ORDER BY legality rules are PG's
    // 42P10 INVALID_COLUMN_REFERENCE, and a set-operation arity mismatch is
    // 42601. Ahead of the variant short-circuits for the same reason the
    // window arm below is: these arrive as `Unsupported`, whose Display
    // prefixes the message.
    // v7.39 (round 299, E3 Phase 2) — a wait that ran out of
    // `lock_timeout` is PG's 55P03, same class as NOWAIT; a wait-for
    // cycle is 40P01, which clients retry.
    if let EngineError::LockDeadlock = e {
        return ("40P01", e.to_string());
    }
    {
        let msg = e.to_string();
        if msg.contains("canceling statement due to lock timeout") {
            return ("55P03", msg);
        }
    }
    // v7.39 (round 297, E3 Phase 1b) — `FOR UPDATE NOWAIT` on a row
    // another transaction holds is PG's 55P03 LOCK_NOT_AVAILABLE.
    // Clients catch that code specifically to back off and retry, so
    // reporting the generic 42000 would be caught by nothing.
    {
        let msg = e.to_string();
        if msg.contains("could not obtain lock on row in relation") {
            return ("55P03", msg);
        }
    }
    {
        let msg = e.to_string();
        if msg.contains("is not in select list")
            || msg.contains("must appear in select list")
            || msg.contains("must match initial ORDER BY expressions")
        {
            return ("42P10", msg);
        }
        // v7.39 (round 240) — the other two ON CONFLICT refusals: touching
        // the same row twice in one command is 21000 CARDINALITY_VIOLATION,
        // and DO UPDATE without a conflict target is 42601.
        if msg.contains("cannot affect row a second time") {
            return ("21000", msg);
        }
        // v7.39 (round 241) — a qualifier naming no table in scope is PG's
        // 42P01 UNDEFINED_TABLE, same class as a missing relation.
        if msg.contains("missing FROM-clause entry for table") {
            return ("42P01", msg);
        }
        // v7.39 (round 242) — grouping() over a non-key is PG's 42803
        // GROUPING_ERROR. Parser-raised, so ahead of the Parse→42601
        // short-circuit.
        if msg.contains("arguments to GROUPING must be grouping expressions") {
            return ("42803", msg);
        }
        // v7.39 (round 244) — sequence-range errors: a setval outside the
        // range is 22003 NUMERIC_VALUE_OUT_OF_RANGE, the CREATE SEQUENCE
        // option refusals 22023.
        if msg.contains("is out of bounds for sequence") {
            return ("22003", msg);
        }
        if msg.contains("cannot be less than MINVALUE")
            || msg.contains("cannot be greater than MAXVALUE")
            || msg.contains("INCREMENT must not be zero")
        {
            return ("22023", msg);
        }
        if msg.contains("ON CONFLICT DO UPDATE requires inference specification") {
            return ("42601", msg);
        }
        if msg.contains("query must have the same number of columns") {
            return ("42601", msg);
        }
        // v7.39 (round 239) — the row-count clause errors come from the
        // parser, so they must be classified ahead of the Parse
        // short-circuit below: a negative LIMIT is PG's 2201W, a negative
        // OFFSET 2201X, and a literal that won't coerce to bigint 22P02.
        if msg.contains("LIMIT must not be negative") {
            return ("2201W", msg);
        }
        if msg.contains("OFFSET must not be negative") {
            return ("2201X", msg);
        }
        if msg.contains("invalid input syntax for type bigint") {
            return ("22P02", msg);
        }
        // v7.39 (round 233) — two set-operation branch columns with no
        // common type are PG's 42804 DATATYPE_MISMATCH.
        if msg.contains(" types ") && msg.contains(" cannot be matched") {
            return ("42804", msg);
        }
    }
    // v7.39 (read01 round 230) — window-clause errors carry PG's own class
    // and must be classified BEFORE the two variant-level short-circuits
    // below: the named-window complaints are raised by the parser (which
    // would otherwise blanket them as 42601) and the frame ones arrive as
    // `Unsupported` (whose Display prefixes "unsupported: ", so the message
    // arms further down only ever see a substring).
    if let Some(code) = window_sqlstate(&e.to_string()) {
        return (code, e.to_string());
    }
    // v7.39 (read01 round 95) — a parse failure is PG's 42601 SYNTAX_ERROR
    // (was the generic 42000). The character position rides the separate `P`
    // field (see parse_error_position).
    if let EngineError::Parse(_) = e {
        return ("42601", e.to_string());
    }
    let msg = e.to_string();
    // Map constraint violations to their PG SQLSTATE class-23 codes so
    // clients can branch on them (23505 for a duplicate key, 23502 for a
    // NOT NULL, 23503 for a foreign key, 23514 for a CHECK) instead of the
    // generic 42000. Match the engine's violation phrasings; the
    // "violation" / "NOT NULL column" qualifiers keep DDL errors that merely
    // mention a constraint kind from being misclassified.
    let code =
        // v7.37.17 (Phase E3) — isolation switch after the tx's first
        // query: PG's 25001 ACTIVE_SQL_TRANSACTION.
        if msg.contains("must be called before any query") {
            "25001"
        // v7.39 (bpchar epic) — CHAR(n)/VARCHAR(n) overflow is PG's 22001
        // STRING_DATA_RIGHT_TRUNCATION.
        } else if msg.contains("value too long for type") {
            "22001"
        // v7.39 (GUC knife 5) — PG's datetime input errors: field values
        // that don't fit the calendar/DateStyle are 22008
        // DATETIME_FIELD_OVERFLOW; malformed text is 22007
        // INVALID_DATETIME_FORMAT.
        } else if msg.contains("date/time field value out of range")
            // v7.39 (read01 timestamp.c) — arithmetic range family.
            || msg.contains("timestamp out of range")
            || msg.contains("interval out of range")
        {
            "22008"
        } else if msg.contains("invalid input syntax for type date")
            || msg.contains("invalid input syntax for type timestamp")
        {
            "22007"
        // v7.39 (read01 utils/adt) — the generic bad-literal class
        // (boolean, money, …) is 22P02 INVALID_TEXT_REPRESENTATION.
        } else if msg.contains("invalid input syntax for type")
            || msg.contains("invalid Roman numeral")
            || msg.contains("invalid cidr value")
        {
            "22P02"
        // v7.39 (read01 utils/adt, float.c) — inverse-trig domain
        // violations (asind(2)) are 22003 NUMERIC_VALUE_OUT_OF_RANGE.
        } else if msg.contains("input is out of range")
            || msg.contains("integer out of range")
            || msg.contains("smallint out of range")
            || msg.contains("bigint out of range")
            || msg.contains("value overflows numeric format")
            || msg.contains("OID out of range")
            || msg.contains("is out of range for type double precision")
            || msg.contains("is out of range for type real")
            // v7.39 (read01 orderedsetaggs.c) — percentile fraction range.
            || msg.contains("is not between 0 and 1")
            // r193 — numeric(p,s) precision overflow (PG's exact text).
            || msg.contains("numeric field overflow")
        {
            "22003"
        // v7.39 (read01 int.c) — PG's 22012 DIVISION_BY_ZERO.
        } else if msg.contains("division by zero") {
            "22012"
        // v7.39 (read01 numeric.c) — log/power domain violations carry
        // their SQL-spec-mandated states.
        // v7.39 (read01 rangetypes.c) — range construction rejections are
        // PG's 22000 DATA_EXCEPTION.
        } else if msg.contains("range lower bound must be less than or equal")
            || msg.contains("result of range difference would not be contiguous")
            || msg.contains("result of range union would not be contiguous")
        {
            "22000"
        } else if msg.contains("malformed range literal")
            || msg.contains("is not a valid binary digit")
        {
            "22P02"
        // v7.39 (read01 varbit.c) — bit-string length mismatch family.
        } else if msg.contains("bit strings of different sizes") {
            "22026"
        // v7.39 (read01 varlena.c) — 22011 SUBSTRING_ERROR.
        } else if msg.contains("negative substring length not allowed") {
            "22011"
        // v7.39 (read01 varlena.c) — byte/bit index out of range.
        } else if msg.contains("out of valid range, 0..") {
            "22003"
        // v7.39 (read01 regexp.c) — 2201B INVALID_REGULAR_EXPRESSION.
        } else if msg.contains("invalid regular expression") {
            "2201B"
        // v7.39 (read01 regproc.c) — name-resolution errors.
        } else if msg.contains("more than one function named")
            || msg.contains("more than one operator named")
        {
            "42725"
        } else if (msg.contains("type \"") && msg.contains("\" does not exist"))
            || msg.contains("text search configuration \"")
            || msg.contains("text search dictionary \"")
            // v7.39 (read01 round 89) — a missing index is PG's 42704
            // UNDEFINED_OBJECT (DROP INDEX / pg_get_indexdef on a bad name).
            || (msg.contains("index \"") && msg.contains("\" does not exist"))
        {
            "42704"
        // v7.39 (read01 round 89) — a column named twice in an INSERT target
        // list is PG's 42701 DUPLICATE_COLUMN.
        } else if msg.contains("\" specified more than once") {
            "42701"
        } else if (msg.contains("function \"") && msg.contains("\" does not exist"))
            || msg.contains("operator does not exist:")
            // v7.39 (read01 round 77) — a named argument aimed at a function
            // that declares no such parameter is PG's 42883 too ("function
            // lpad(string => unknown, …) does not exist"): no candidate matches
            // the call. SPG names the reason instead of the missing candidate,
            // but a driver must still read the same class.
            || msg.contains("does not support named arguments")
            || msg.contains("has no argument named")
        {
            "42883"
        // v7.39 (read01 round 45, commands/) — DDL object errors.
        // A second PRIMARY KEY is PG's 42P16 INVALID_TABLE_DEFINITION.
        } else if msg.contains("multiple primary keys for table") {
            "42P16"
        // A GENERATED ALWAYS identity/column explicit-value insert is PG's
        // 428C9 GENERATED_ALWAYS.
        } else if msg.contains("cannot insert a non-DEFAULT value into column") {
            "428C9"
        // A duplicate column on ALTER TABLE ADD COLUMN is 42701
        // DUPLICATE_COLUMN; a missing column is 42703 UNDEFINED_COLUMN.
        } else if msg.contains("column \"") && msg.contains("already exists") {
            "42701"
        } else if msg.contains("column \"") && msg.contains("does not exist") {
            "42703"
        // v7.39 (read01 round 47) — constraint errors must be classified
        // BEFORE the table/relation patterns below: PG's RENAME CONSTRAINT
        // wording ("constraint \"c\" for table \"t\" does not exist") also
        // contains `table "`, which would otherwise steal it for 42P01.
        // A duplicate object (constraint / type) is 42710; a missing one is
        // 42704 UNDEFINED_OBJECT.
        // The dup-constraint pattern must be narrow: PG's 23505 duplicate-key
        // message also carries `constraint "t_pkey"` and a DETAIL ending in
        // "already exists.", so key on PG's distinctive "for relation" /
        // "for table" qualifier, which only the DDL form has.
        } else if msg.contains("constraint \"")
            && (msg.contains("\" for relation \"") || msg.contains("\" for table \""))
            && msg.contains("already exists")
        {
            "42710"
        } else if msg.contains("constraint \"") && msg.contains("does not exist") {
            "42704"
        } else if msg.contains("type \"") && msg.contains("already exists") {
            "42710"
        // v7.39 (read01 round 49) — ALTER TYPE ADD VALUE / RENAME VALUE.
        } else if msg.contains("enum label \"") && msg.contains("already exists") {
            "42710"
        // v7.39 (read01 round 235) — jsonpath strict-mode refusals each
        // carry their own SQLSTATE in PG's SQL/JSON classes, not one
        // shared code: a missing key / non-object accessor is 2203A
        // SQL_JSON_MEMBER_NOT_FOUND, an out-of-range subscript 22033
        // INVALID_SQL_JSON_SUBSCRIPT, a wildcard on a non-array 22039
        // SQL_JSON_ARRAY_NOT_FOUND.
        } else if msg.contains("JSON object does not contain key")
            || msg.contains("jsonpath member accessor can only be applied")
        {
            "2203A"
        } else if msg.contains("jsonpath array subscript is out of bounds") {
            "22033"
        } else if msg.contains("jsonpath wildcard array accessor can only be applied")
            || msg.contains("jsonpath array accessor can only be applied")
        {
            "22039"
        } else if msg.contains("is not an existing enum label")
            // v7.39 (read01 round 234) — the jsonb modification family's
            // refusals are PG's 22023 INVALID_PARAMETER_VALUE too.
            || msg.contains("cannot delete from scalar")
            || msg.contains("cannot delete path in scalar")
            || msg.contains("cannot set path in scalar")
            || msg.contains("cannot delete from object using integer index")
        {
            "22023"
        // DROP IDENTITY on a plain column.
        } else if msg.contains("is not an identity column") {
            "42703"
        // A duplicate relation (table / index / view / sequence) is 42P07.
        } else if msg.contains("relation \"") && msg.contains("already exists") {
            "42P07"
        // DROP TABLE on a missing table is 42P01 UNDEFINED_TABLE; every
        // other path (SELECT / ALTER / …) says "relation", same state.
        } else if (msg.contains("table \"")
            || msg.contains("relation \"")
            // v7.39 (read01 round 89) — a missing view is PG's 42P01 too.
            || msg.contains("view \""))
            && msg.contains("does not exist")
        {
            "42P01"
        } else if msg.contains("cannot take logarithm of") {
            "2201E"
        } else if msg.contains("zero raised to a negative power is undefined")
            || msg.contains("a negative number raised to a non-integer power")
            || msg.contains("cannot take square root of a negative number")
        {
            "2201F"
        // v7.39 (read01 json.c) — 22030 DUPLICATE_JSON_OBJECT_KEY_VALUE.
        } else if msg.contains("duplicate JSON object key value") {
            "22030"
        // v7.39 (read01 like_match.c) — 22025 INVALID_ESCAPE_SEQUENCE.
        } else if msg.contains("LIKE pattern must not end with escape")
            || msg.contains("invalid escape string")
        {
            "22025"
        // v7.39 (read01 oracle_compat.c) — chr() limits are PG's 54000
        // PROGRAM_LIMIT_EXCEEDED.
        } else if msg.contains("null character not permitted")
            || msg.contains("requested character too large for encoding")
        {
            "54000"
        // v7.39 (tz epic) — bad GUC values (TimeZone / DateStyle /
        // IntervalStyle / extra_float_digits range) are PG's 22023
        // INVALID_PARAMETER_VALUE.
        } else if msg.contains("invalid value for parameter")
            || msg.contains("is outside the valid range for parameter")
            || msg.contains("sample size must be between")
            // v7.39 (ts_headline validation) — PG's headline option
            // errors are 22023 INVALID_PARAMETER_VALUE.
            || msg.contains("unrecognized headline parameter")
            || msg.contains("MinWords must be")
            || msg.contains("ShortWord must be")
            || msg.contains("MaxFragments must be")
            || msg.contains("step size cannot equal zero")
            || msg.contains("field position must not be zero")
            // v7.39 (read01 numeric.c) — generate_series(numeric) bound /
            // step rejections.
            || msg.contains("start value cannot be")
            || msg.contains("stop value cannot be")
            || msg.contains("step size cannot be")
            || msg.contains("cannot get array length of")
            || msg.contains("cannot call json_object_keys")
            || msg.contains("cannot call jsonb_object_keys")
            || msg.contains("string is not a valid identifier")
            // v7.39 (read01 round 51) — has_table_privilege's privilege word.
            || msg.contains("unrecognized privilege type")
            // v7.39 (round 253) — an unknown EXTRACT/date_part field name.
            || (msg.contains("unit \"") && msg.contains("\" not recognized for type"))
        {
            "22023"
        // v7.39 (ts_headline validation) — a malformed key=value list is
        // PG's 42601 SYNTAX_ERROR.
        } else if msg.contains("invalid parameter list format")
            || msg.contains("of jsonpath input")
            // v7.39 (read01 round 88) — INSERT value/column arity mismatch is
            // PG's 42601 SYNTAX_ERROR.
            || msg.contains("INSERT has more expressions than target columns")
            || msg.contains("INSERT has more target columns than expressions")
        {
            "42601"
        // v7.39 (read01 utils/adt) — PG's multidim-search refusal is
        // 0A000 FEATURE_NOT_SUPPORTED.
        } else if msg.contains("searching for elements in multidimensional arrays")
            || msg.contains("encoding conversion from UTF8 to ASCII")
            // v7.39 (read01 pseudotypes.c) — dummy pseudotype input funcs.
            || msg.contains("cannot accept a value of type")
            // v7.39 (round 150) — referencing a no-RETURNING data-modifying
            // CTE (parse_relation.c addRangeTableEntryForCTE).
            || msg.contains("does not have a RETURNING clause")
            // v7.39 (round 151) — the modifying-CTE placement rules: nested
            // WITH (parse_cte.c) and view / matview bodies (view.c,
            // analyze.c). Round 81 introduced the first message with the
            // then-default 42000; PG uses 0A000 for all three.
            || msg.contains("must be at the top level")
            || msg.contains("must not contain data-modifying statements in WITH")
            || msg.contains("must not use data-modifying statements in WITH")
            // v7.39 (round 154) — writes targeting a computed view column.
            || msg.contains("View columns that are not columns of their base relation")
            // v7.39 (round 247) — the COPY option refusals share the class.
            || msg.contains("requires CSV mode")
            || msg.contains("must be a single one-byte character")
            // v7.39 (round 253) — EXTRACT field/type validity (PG 0A000).
            || (msg.contains("unit \"") && msg.contains("\" not supported for type"))
        {
            "0A000"
        } else if msg.contains("duplicate key value violates unique constraint")
            || (msg.contains("violation") && (msg.contains("UNIQUE") || msg.contains("PRIMARY KEY")))
            // v7.39 (read01 round 52) — CREATE UNIQUE INDEX over duplicate rows.
            || msg.contains("could not create unique index")
        {
            "23505"
        // v7.39 (round 210) — EXCLUDE constraint violation (PG 23P01
        // exclusion_violation).
        } else if msg.contains("violates exclusion constraint") {
            "23P01"
        // v7.39 (round 220) — a CYCLE-less sequence past its bound
        // (PG 2200H sequence_generator_limit_exceeded).
        } else if msg.contains("nextval: reached") {
            "2200H"
        } else if msg.contains("violates foreign key constraint")
            || msg.contains("FOREIGN KEY violation")
        {
            "23503"
        } else if msg.contains("violates check option") {
            // v7.39 (round 132) — WITH CHECK OPTION violation.
            "44000"
        } else if msg.contains("violates check constraint")
            || msg.contains("CHECK constraint violation")
        {
            "23514"
        } else if msg.contains("violates not-null constraint")
            || msg.contains("NOT NULL column")
            // v7.39 (read01 round 49) — SET NOT NULL over existing NULLs.
            || msg.contains("contains null values")
        {
            "23502"
        // v7.39 (SQLSTATE fidelity) — file-access failures map like
        // PG's errcode_for_file_access(): ENOSPC/EDQUOT -> 53100
        // disk_full, ENOMEM -> 53200 out_of_memory, EACCES/EPERM ->
        // 42501 insufficient_privilege, ENOENT -> 58P01
        // undefined_file, anything else on the durability path ->
        // 58030 io_error. Matched on the OS message the io::Error
        // Display carries.
        } else if msg.contains("durability append failed") || msg.contains("could not write") {
            let lower = msg.to_ascii_lowercase();
            if lower.contains("no space left")
                || lower.contains("quota")
                || lower.contains("storage full")
                || lower.contains("below water-mark")
            {
                "53100"
            } else if lower.contains("out of memory") {
                "53200"
            } else if lower.contains("permission denied") {
                "42501"
            } else if lower.contains("no such file") {
                "58P01"
            } else {
                "58030"
            }
        // v7.39 (read01 round 57) — the table-privilege failures PG raises as
        // 42501 insufficient_privilege, and the unknown-role one (42704).
        } else if msg.contains("permission denied for table")
            || msg.contains("permission denied for sequence")
            || msg.contains("permission denied for schema")
            || msg.contains("permission denied for function")
            || msg.contains("must be owner of table")
        {
            "42501"
        } else if msg.contains("role \"") && msg.contains("does not exist") {
            "42704"
        // v7.39 (read01 round 58) — DROP ROLE with grants still pointing at it
        // (PG 2BP01 dependent_objects_still_exist).
        } else if msg.contains("cannot be dropped because some objects depend on it") {
            "2BP01"
        // v7.39 (read01 round 62) — an overloaded name with no signature to
        // disambiguate it (PG 42725 ambiguous_function).
        } else if msg.contains("is not unique") {
            "42725"
        // …and a call / GRANT / DROP naming a function that has no such
        // signature (PG 42883 undefined_function).
        } else if msg.contains("function") && msg.contains("does not exist") {
            "42883"
        } else {
            "42000"
        };
    // v7.39 (read01 round 79) — PG's ErrorResponse carries the message ALONE.
    // SPG's `Display for EngineError` prefixes the internal error class
    // ("eval: type mismatch: …", "unsupported: …", "parse: …"), which is useful
    // in a Rust backtrace and is noise — and a visible non-PG-ism — on the wire:
    // every error a client saw was prefixed with SPG's own vocabulary. Strip the
    // class here, at the boundary, so the Rust-facing Display keeps it. The
    // SQLSTATE classification above matched on the full string, so it is
    // unaffected.
    let msg = strip_error_class(&msg);
    (code, msg)
}

#[cfg(test)]
mod engine_error_sqlstate_tests {
    use super::engine_error_to_wire;
    use spg_engine::EngineError;

    fn code(msg: &str) -> &'static str {
        engine_error_to_wire(&EngineError::Unsupported(msg.to_string())).0
    }

    #[test]
    fn constraint_violations_map_to_class_23() {
        // v7.39 (SQLSTATE fidelity) — the engine speaks PG's 23505
        // phrasing now; the legacy phrasings stay classified too.
        assert_eq!(
            code(
                "duplicate key value violates unique constraint \"t_pkey\" on table \"t\" \
                 DETAIL: Key (id)=(1) already exists."
            ),
            "23505"
        );
        assert_eq!(
            code(
                "PRIMARY KEY violation on \"t\" columns [\"id\"]: row #0 duplicates an existing key"
            ),
            "23505"
        );
        assert_eq!(
            code("UNIQUE INDEX \"i\" violation on \"t\": row #0 duplicates"),
            "23505"
        );
        assert_eq!(
            code("FOREIGN KEY violation: no parent row in \"t\" where id = Int(9)"),
            "23503"
        );
        // v7.39 (round 210) — EXCLUDE violation maps to PG's 23P01.
        assert_eq!(
            code(
                "conflicting key value violates exclusion constraint \"ov_during_excl\" \
                 on table \"ov\" DETAIL: Key (during)=([3,7)) conflicts with existing \
                 key (during)=([1,5))."
            ),
            "23P01"
        );
        assert_eq!(
            code("CHECK constraint violation on \"t\" (row #0): \"(y > 0)\""),
            "23514"
        );
        // v7.39 (round 132) — WITH CHECK OPTION violation maps to 44000.
        assert_eq!(
            code("new row violates check option for view \"vv\""),
            "44000"
        );
        // NOT NULL flows through Storage(NullInNotNull) whose Display is
        // "NULL value in NOT NULL column …".
        assert_eq!(
            code("storage: NULL value in NOT NULL column \"x\""),
            "23502"
        );
        // A DDL error mentioning a constraint kind is not a violation.
        assert_eq!(
            code("cannot add UNIQUE constraint to column with duplicate data"),
            "42000"
        );
        assert_eq!(code("syntax error near \"FROM\""), "42000");
        // v7.39 — errno family on the durability path.
        assert_eq!(
            code("durability append failed: No space left on device (os error 28)"),
            "53100"
        );
        assert_eq!(
            code("durability append failed: WAL append hit storage full"),
            "53100"
        );
        assert_eq!(
            code("durability append failed: Permission denied (os error 13)"),
            "42501"
        );
        assert_eq!(
            code("durability append failed: No such file or directory (os error 2)"),
            "58P01"
        );
        assert_eq!(code("durability append failed: broken pipe"), "58030");
        // v7.39 (read01 round 45, commands/) — DDL object errors.
        assert_eq!(
            code("multiple primary keys for table \"t3\" are not allowed"),
            "42P16"
        );
        assert_eq!(
            code("column \"a\" of relation \"t2\" already exists"),
            "42701"
        );
        assert_eq!(
            code("column \"nonexist\" of relation \"t2\" does not exist"),
            "42703"
        );
        assert_eq!(code("table \"nonexist_tbl\" does not exist"), "42P01");
        assert_eq!(
            code(
                "cannot insert a non-DEFAULT value into column \"a\" \
                 DETAIL: Column \"a\" is an identity column defined as GENERATED ALWAYS.\n\
                 HINT:  Use OVERRIDING SYSTEM VALUE to override."
            ),
            "428C9"
        );
        // v7.39 (read01 round 47) — the rest of the DDL object-error surface.
        assert_eq!(code("relation \"r1\" already exists"), "42P07");
        assert_eq!(code("relation \"nope_tbl\" does not exist"), "42P01");
        assert_eq!(code("type \"r_enum\" already exists"), "42710");
        assert_eq!(
            code("constraint \"c1\" for relation \"r1\" already exists"),
            "42710"
        );
        assert_eq!(
            code("constraint \"nope\" of relation \"r1\" does not exist"),
            "42704"
        );
        assert_eq!(code("column \"nope\" does not exist"), "42703");
        // The 23505 duplicate-key message also mentions a constraint and
        // "already exists" — it must NOT be stolen by the 42710 pattern.
        assert_eq!(
            code(
                "duplicate key value violates unique constraint \"t_pkey\" on table \"t\" \
                 DETAIL: Key (id)=(1) already exists."
            ),
            "23505"
        );
    }
}

// ---- v4.17 COPY FROM STDIN / COPY TO STDOUT ----

#[derive(Debug)]
enum CopyIntent {
    // v7.39 (read01 round 91) — the explicit `(col, …)` list, captured (was
    // parsed-and-discarded) so the row-arity check knows how many values each
    // COPY row must carry and can name a missing column.
    From(String, Option<Vec<String>>, CopyOptions),
    // v7.39 (read01 round 94) — `To` now carries the parsed options (was
    // silently dropped: `COPY t TO STDOUT WITH (FORMAT csv, HEADER)` streamed
    // plain text). `ToQuery` is the `COPY (<query>) TO STDOUT` form — the
    // inner SQL is run and its result set streamed in COPY format.
    To(String, CopyOptions),
    ToQuery(String, CopyOptions),
    // v7.39 (round 251) — `COPY t FROM '<file>'`: the SERVER process
    // reads the file, PG semantics. Parsed by the engine's SQL parser
    // (spg_engine::copy::parse_copy_from_file), not the hand parser, so
    // the r247 option grammar (one-byte checks, mode refusals) applies.
    FromFile(spg_engine::copy::CopyFromFileSpec),
    // v7.39 (round 252) — `COPY … TO '<file>'`: the SERVER renders via
    // the engine and writes the file (PG semantics, pg_write_server_files
    // analog: admin only).
    ToFile(spg_engine::copy::CopyToFileSpec),
    /// v7.39 (round 265) — an unrecognized option name. Carried as an
    /// intent so the COPY dispatch can raise PG's `option "x" not
    /// recognized` instead of the previous silent accept, which made the
    /// statement report success while the option did nothing.
    BadOption(String),
}

/// v6.4.7 — `COPY FROM STDIN WITH (...)` option parser. PG-style
/// comma-separated `key value` pairs inside parens.
#[derive(Debug, Clone, Default)]
struct CopyOptions {
    /// `SKIP n` — drop the first N data rows (typically the CSV
    /// header row).
    pub skip: u64,
    /// `ON_ERROR SET_NULL` — on per-cell parse failure, replace the
    /// failed cell with NULL instead of aborting the COPY. The row
    /// is still rejected (with a clear message) if the failed cell
    /// targets a NOT NULL column.
    pub on_error_set_null: bool,
    /// `FORMAT JSON` — each input line is a JSON object whose keys
    /// match the target table's column names. Missing columns become
    /// NULL; extra keys are ignored. Default (no FORMAT) is the
    /// existing tab-delimited text mode.
    pub format_json: bool,
    /// `FORMAT CSV` — RFC-4180-style records: quoted fields, doubled
    /// quotes, embedded delimiters/newlines inside quotes, and an
    /// unquoted empty field reads as NULL while `""` is the empty
    /// string. Mutually exclusive with `format_json`.
    pub format_csv: bool,
    /// CSV `DELIMITER 'c'` (default `,`).
    pub csv_delimiter: Option<char>,
    /// CSV `QUOTE 'c'` (default `"`).
    pub csv_quote: Option<char>,
    /// `NULL 'token'` — the text that decodes to NULL. CSV default is the
    /// empty unquoted field; text default is `\N`. v7.39 (read01 round 91).
    pub null_string: Option<String>,
    /// `HEADER` on the `TO STDOUT` direction — emit a leading row of column
    /// names (the FROM direction reuses `skip` to drop it instead). v7.39
    /// (read01 round 94). Kept separate from `skip` because the same option
    /// word means "emit" going out and "drop" coming in.
    pub header: bool,
}

/// Detects `COPY <table> [(col1, col2, …)] FROM STDIN [WITH
/// (options)]` and `COPY <table> [(…)] TO STDOUT`
/// (case-insensitive). Anything else (e.g. `COPY ... FROM
/// '/path'`) falls through to the regular engine path, which
/// will report a parse error — file-based COPY is intentionally
/// not supported (no filesystem access from the server in the
/// docker-compose deployment shape).
///
/// v7.15.0 — the column-list form `COPY t (a, b, c) FROM STDIN`
/// is recognised. pg_dump emits this for every table with data
/// (the default output, not just `--column-inserts`), so without
/// this we mis-classify any `pg_dump` (no `--schema-only`) →
/// `psql -f` flow as a normal SQL statement and report a parse
/// error.
fn parse_copy_intent(sql: &str) -> Option<CopyIntent> {
    let trimmed = sql.trim();
    if !ci_starts_with(trimmed.as_bytes(), b"copy ") {
        return None;
    }
    let lower = trimmed.to_ascii_lowercase();
    let rest = lower.strip_prefix("copy ")?;
    // Walk the prefix manually so we can skip an optional
    // parenthesised column list between the table name and the
    // FROM/TO keyword. Token splitting on whitespace alone
    // mistakes `(col1,` for the FROM direction word.
    let bytes = rest.as_bytes();
    let mut i = skip_ws_bytes(bytes, 0);
    // v7.39 (read01 round 94) — `COPY (<query>) TO STDOUT`. When the first
    // token after COPY is `(` there is no table name; the parens wrap a
    // SELECT/VALUES/WITH whose result set is streamed. (The `COPY t (a,b)`
    // column-list form has a table name first, so it never lands here.)
    if bytes.get(i) == Some(&b'(') {
        return parse_copy_query_intent(trimmed, rest, i);
    }
    // Table name (may be schema-qualified `s.t`, MySQL-style
    // backtick-quoted, or PG double-quoted). Read until the next
    // whitespace OR `(`.
    let table_start = i;
    while i < bytes.len() {
        let c = bytes[i] as char;
        if c.is_ascii_whitespace() || c == '(' {
            break;
        }
        i += 1;
    }
    if i == table_start {
        return None;
    }
    // Strip optional `<schema>.` qualifier — pg_dump emits
    // `public.posts`. The SPG SQL parser does the same strip; do
    // the COPY-path equivalent here so the catalog lookup hits
    // the bare table name.
    let raw = &rest[table_start..i];
    let table = match raw.rsplit_once('.') {
        Some((_, bare)) => bare.to_string(),
        None => raw.to_string(),
    };
    // Skip an optional `(col, col, …)` column list. v7.15.0
    // doesn't need the column names — the COPY data path
    // builds INSERTs against the table's full column list (or
    // a JSON-keyed subset) by default. Recording the column
    // list per-COPY is a v7.15.x follow-up if mismatched-arity
    // dumps surface.
    i = skip_ws_bytes(bytes, i);
    let mut column_list: Option<Vec<String>> = None;
    if bytes.get(i) == Some(&b'(') {
        let list_start = i + 1;
        let mut depth = 1usize;
        i += 1;
        while i < bytes.len() && depth > 0 {
            match bytes[i] {
                b'(' => depth += 1,
                b')' => depth -= 1,
                _ => {}
            }
            i += 1;
        }
        // `i-1` is the matching ')'. Take the names from the ORIGINAL-case SQL.
        // `rest` is the lowercased tail after "copy "; map the offsets onto the
        // trimmed original by the same prefix length.
        let prefix = trimmed.len() - rest.len();
        let names: Vec<String> = trimmed[prefix + list_start..prefix + i - 1]
            .split(',')
            .map(|c| c.trim().trim_matches('"').to_string())
            .filter(|c| !c.is_empty())
            .collect();
        if !names.is_empty() {
            column_list = Some(names);
        }
        i = skip_ws_bytes(bytes, i);
    }
    // dir + endpoint, whitespace-separated.
    let dir_start = i;
    while i < bytes.len() && !(bytes[i] as char).is_ascii_whitespace() {
        i += 1;
    }
    if i == dir_start {
        return None;
    }
    let dir = &rest[dir_start..i];
    i = skip_ws_bytes(bytes, i);
    let ep_start = i;
    while i < bytes.len() && !(bytes[i] as char).is_ascii_whitespace() && bytes[i] != b';' {
        i += 1;
    }
    if i == ep_start {
        return None;
    }
    let endpoint = &rest[ep_start..i];
    // v7.39 (round 251/252) — a quoted endpoint is the file form; the
    // engine's SQL parser owns the grammar.
    if dir == "from" && endpoint.starts_with('\'') {
        return spg_engine::copy::parse_copy_from_file(trimmed).map(CopyIntent::FromFile);
    }
    if dir == "to" && endpoint.starts_with('\'') {
        return spg_engine::copy::parse_copy_to_file(trimmed).map(CopyIntent::ToFile);
    }
    match (dir, endpoint) {
        ("from", "stdin") => {
            // v7.39 (read01 round 91) — parse options from the ORIGINAL-case
            // SQL: option VALUES like `NULL 'NULLTOKEN'` / DELIMITER '|' are
            // case-sensitive (the null token must match the data byte-for-byte),
            // so only KEYS get lowercased inside the parser.
            match parse_copy_options_checked(trimmed) {
                Ok(opts) => Some(CopyIntent::From(table, column_list, opts)),
                Err(bad) => Some(CopyIntent::BadOption(bad)),
            }
        }
        ("to", "stdout") => {
            match parse_copy_options_checked(trimmed) {
                Ok(opts) => Some(CopyIntent::To(table, opts)),
                Err(bad) => Some(CopyIntent::BadOption(bad)),
            }
        }
        _ => None,
    }
}

/// v7.39 (read01 round 94) — parse `COPY (<query>) TO STDOUT [WITH (…)]`.
/// `lparen` is the byte offset of the opening `(` within `rest` (the
/// lower-cased tail after `copy `); `trimmed` is the original-case SQL so
/// the inner query keeps its case. Returns `None` (falls through to the
/// normal parse-error path) if the parens are unbalanced or `TO STDOUT`
/// doesn't follow.
fn parse_copy_query_intent(trimmed: &str, rest: &str, lparen: usize) -> Option<CopyIntent> {
    let bytes = rest.as_bytes();
    // Find the matching close paren for the wrapping `(`.
    let mut depth = 0usize;
    let mut j = lparen;
    let mut close = None;
    while j < bytes.len() {
        match bytes[j] {
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    close = Some(j);
                    break;
                }
            }
            _ => {}
        }
        j += 1;
    }
    let close = close?;
    // Inner query, taken from the ORIGINAL-case SQL (offsets map through the
    // shared `copy ` prefix length).
    let prefix = trimmed.len() - rest.len();
    let query = trimmed[prefix + lparen + 1..prefix + close]
        .trim()
        .to_string();
    if query.is_empty() {
        return None;
    }
    // After the `)` we require `TO STDOUT`; anything else (e.g. `TO '/file'`)
    // falls through to the engine, which reports it honestly.
    let after = &rest[close + 1..];
    let lower_after = after.trim_start();
    let mut it = lower_after.split_ascii_whitespace();
    if !matches!(it.next(), Some("to")) {
        return None;
    }
    // `stdout` may be trailed by `;` or a `WITH (...)` clause.
    let ep = it.next().unwrap_or("");
    // v7.39 (round 252) — a quoted endpoint is the file form; the
    // engine's SQL parser owns the grammar (options included).
    if ep.starts_with('\'') {
        return spg_engine::copy::parse_copy_to_file(trimmed).map(CopyIntent::ToFile);
    }
    if ep.trim_end_matches(';') != "stdout" {
        return None;
    }
    // Options come from the tail after the wrapping `)` so the query's own
    // parens/`WITH` CTE can't be mistaken for the COPY option list.
    let opts = parse_copy_options(&trimmed[prefix + close + 1..]);
    Some(CopyIntent::ToQuery(query, opts))
}

fn skip_ws_bytes(bytes: &[u8], mut i: usize) -> usize {
    while i < bytes.len() && (bytes[i] as char).is_ascii_whitespace() {
        i += 1;
    }
    i
}

/// Find a `WITH (...)` chunk in the SQL and decode the options.
/// v7.39 (round 265) — returns the parsed options, or the name of the
/// first UNRECOGNIZED one. The catch-all used to swallow anything it did
/// not know, so `COPY … WITH (NOSUCHOPT true)` reported success and the
/// option simply did nothing — the same silent-accept shape round 260
/// closed for ALTER DOMAIN. PG raises `option "x" not recognized`.
fn parse_copy_options(sql: &str) -> CopyOptions {
    parse_copy_options_checked(sql).unwrap_or_else(|_| CopyOptions::default())
}

fn parse_copy_options_checked(sql: &str) -> Result<CopyOptions, String> {
    let mut opts = CopyOptions::default();
    // Find the WITH (...) group. A `NULL '('`-style token could contain a paren,
    // but PG's option grammar keeps these simple; the outer WITH ( … ) is what
    // we split. Find the LAST '(' after "with" to skip a table column list.
    // v7.39 (round 265) — NO `WITH` means no options at all. This used to
    // fall back to position 0 and pick up the TABLE's column list
    // (`COPY t (a,b) FROM STDIN`) as if it were an option group; the old
    // catch-all silently ignored the resulting garbage, so it went
    // unnoticed until unknown options started being rejected.
    let Some(search_from) = sql.to_ascii_lowercase().rfind("with") else {
        return Ok(opts);
    };
    let Some(open) = sql[search_from..].find('(').map(|p| search_from + p) else {
        return Ok(opts);
    };
    let Some(close) = sql[open..].rfind(')').map(|p| open + p) else {
        return Ok(opts);
    };
    let inner = &sql[open + 1..close];
    for pair in inner.split(',') {
        let pair = pair.trim();
        if pair.is_empty() {
            continue;
        }
        let mut it = pair.split_ascii_whitespace();
        let key = it.next().unwrap_or("").to_ascii_lowercase();
        let val_raw = it.next().unwrap_or("");
        let val_lc = val_raw.to_ascii_lowercase();
        let (key, val) = (key.as_str(), val_lc.as_str());
        match key {
            "skip" => {
                opts.skip = val.parse().unwrap_or(0);
            }
            "on_error" => {
                if val == "set_null" {
                    opts.on_error_set_null = true;
                }
            }
            "format" => match val {
                "json" => opts.format_json = true,
                "csv" => opts.format_csv = true,
                _ => {}
            },
            // `HEADER [true|on]` (or bare `HEADER`) skips the first data
            // row — the CSV header line. Reuses the SKIP machinery.
            "header" => {
                if val.is_empty() || val == "true" || val == "on" {
                    opts.skip = opts.skip.max(1);
                    opts.header = true;
                }
            }
            "delimiter" => {
                opts.csv_delimiter = unquote_copy_char(val_raw);
            }
            "quote" => {
                opts.csv_quote = unquote_copy_char(val_raw);
            }
            // v7.39 (read01 round 91) — `NULL 'token'`: the string that reads as
            // NULL. Ignored before, so `WITH (NULL 'X')` left the literal "X" in
            // the column instead of a NULL.
            "null" => {
                let t = val_raw.trim_matches(|c| c == '\'' || c == '"');
                if !t.is_empty() {
                    opts.null_string = Some(t.to_string());
                }
            }
            other => {
                return Err(other.to_ascii_lowercase());
            }
        }
    }
    Ok(opts)
}

/// Strip the surrounding quotes from a COPY option value like `','` or
/// `'#'` and return the single character. Returns `None` if the value
/// is not exactly one character (PG requires single-character DELIMITER
/// / QUOTE).
fn unquote_copy_char(val: &str) -> Option<char> {
    let s = val.trim_matches(|c| c == '\'' || c == '"');
    let mut chars = s.chars();
    let c = chars.next()?;
    if chars.next().is_some() {
        return None;
    }
    Some(c)
}

/// COPY FROM STDIN — server sends CopyInResponse, reads CopyData
/// frames, parses each row (tab-delimited text, `\N` = NULL),
/// inserts via engine.execute("INSERT ..."). CopyDone commits;
/// CopyFail aborts.
/// v7.39 (read01 round 91) — after an error mid-COPY-FROM the client is still
/// streaming CopyData; the server must consume its frames up to CopyDone/CopyFail
/// before it may send ReadyForQuery, or the client's later frames arrive "while
/// idle" and desync the protocol.
fn drain_copy_in_frames(stream: &mut dyn ReadWrite) -> std::io::Result<()> {
    loop {
        let mut header = [0u8; 5];
        if stream.read_exact(&mut header).is_err() {
            return Ok(());
        }
        let ty = header[0];
        let len = u32::from_be_bytes([header[1], header[2], header[3], header[4]]) as usize;
        let body_len = len.saturating_sub(4);
        if body_len > 0 {
            let mut body = vec![0u8; body_len];
            if stream.read_exact(&mut body).is_err() {
                return Ok(());
            }
        }
        // CopyDone / CopyFail end the input stream.
        if ty == b'c' || ty == b'f' {
            return Ok(());
        }
    }
}

/// v7.39 (round 252) — `COPY … TO '<file>'`: the server process writes
/// the file (PG semantics; probed live 2026-07-19). Non-admin roles get
/// PG's 42501; an OS permission failure is ALSO 42501 (PG's class for
/// it, probed), while a missing directory is 58P01 — both with the
/// psql \copy HINT. Read-only: no WAL involvement.
fn handle_copy_to_file(
    stream: &mut dyn ReadWrite,
    state: &Arc<ServerState>,
    role: Role,
    spec: &spg_engine::copy::CopyToFileSpec,
) -> std::io::Result<()> {
    if role != Role::Admin {
        send_error(
            stream,
            "42501",
            "permission denied to COPY to a file DETAIL: Only roles with privileges of the \
             \"pg_write_server_files\" role may COPY to a file.\nHINT:  Anyone can COPY to \
             stdout or from stdin. psql's \\copy command also works for anyone.",
        )?;
        return Ok(());
    }
    let rendered = state
        .engine
        .write()
        .map_err(|_| std::io::Error::other("engine rwlock poisoned"))
        .map(|mut e| {
            e.copy_to_buffer(
                &spec.table,
                spec.columns.as_deref(),
                spec.query.as_deref(),
                &spec.options,
            )
        })?;
    let (payload, n) = match rendered {
        Ok(r) => r,
        Err(e) => {
            let msg = format!("{e}");
            let code = if msg.contains("relation") && msg.contains("does not exist") {
                "42P01"
            } else if msg.contains("does not exist") || msg.contains("column") {
                "42703"
            } else {
                "0A000"
            };
            send_error(stream, code, &msg)?;
            return Ok(());
        }
    };
    if let Err(e) = std::fs::write(&spec.path, payload) {
        let os = e.to_string();
        let os = os.split(" (os error").next().unwrap_or(&os).to_string();
        // PG: an EACCES open is 42501; anything else here is 58P01.
        let code = if e.kind() == std::io::ErrorKind::PermissionDenied {
            "42501"
        } else {
            "58P01"
        };
        send_error(
            stream,
            code,
            &format!(
                "could not open file \"{path}\" for writing: {os}\nHINT:  COPY TO instructs \
                 the PostgreSQL server process to write a file. You may want a client-side \
                 facility such as psql's \\copy.",
                path = spec.path
            ),
        )?;
        return Ok(());
    }
    send_command_complete(stream, &format!("COPY {n}"))?;
    Ok(())
}

/// v7.39 (round 251) — `COPY t [(cols)] FROM '<file>'`: the server
/// process reads the file (PG semantics; probed live 2026-07-19).
/// Non-admin roles get PG's 42501 refusal; a missing file is PG's
/// 58P01 with the psql \copy HINT. The rows drive the same
/// BEGIN / per-row INSERT+WAL / COMMIT-or-ROLLBACK sequence as the
/// v7.39 (round 343, V40) — run a `SELECT lo_import(…)` /
/// `SELECT lo_export(…)`. PG 18.4, measured: the result column is named
/// after the function, `lo_import` answers the new oid and `lo_export`
/// answers `1`; both are superuser-only.
fn handle_lo_file_call(
    wbuf: &mut Vec<u8>,
    state: &Arc<ServerState>,
    role: Role,
    call: &spg_engine::largeobject::LoFileCall,
) -> std::io::Result<()> {
    use spg_engine::largeobject::LoFileCall;
    if role != Role::Admin {
        return send_error(
            wbuf,
            "42501",
            &spg_engine::largeobject::permission_denied(call),
        );
    }
    let value = match call {
        LoFileCall::Import { path, oid } => {
            let data = match std::fs::read(path) {
                Ok(d) => d,
                Err(e) => {
                    return send_error(
                        wbuf,
                        "58P01",
                        &spg_engine::largeobject::could_not_open(path, &e.to_string()),
                    );
                }
            };
            let mut eng = state
                .engine
                .write()
                .map_err(|_| std::io::Error::other("engine rwlock poisoned"))?;
            match eng.lo_import_bytes(oid.unwrap_or(0), data) {
                Ok(new_oid) => i64::from(new_oid),
                Err(e) => return send_error(wbuf, "58P01", &format!("{e}")),
            }
        }
        LoFileCall::Export { oid, path } => {
            let bytes = {
                let eng = state
                    .engine
                    .read()
                    .map_err(|_| std::io::Error::other("engine rwlock poisoned"))?;
                match eng.lo_export_bytes(*oid) {
                    Ok(b) => b,
                    Err(e) => return send_error(wbuf, "42704", &format!("{e}")),
                }
            };
            if let Err(e) = std::fs::write(path, &bytes) {
                return send_error(
                    wbuf,
                    "58P01",
                    &spg_engine::largeobject::could_not_create(path, &e.to_string()),
                );
            }
            1
        }
    };
    send_canned(
        wbuf,
        &CannedResponse::Rows {
            columns: vec![ColumnSchema::new(
                call.column_name().to_string(),
                DataType::BigInt,
                false,
            )],
            rows: vec![Row::new(vec![Value::BigInt(value)])],
        },
    )
}

/// round-250 STDIN path, so the COPY is atomic and durable (one fsync
/// at COMMIT, crash mid-COPY replays to the end-of-WAL auto-rollback).
fn handle_copy_from_file(
    stream: &mut dyn ReadWrite,
    state: &Arc<ServerState>,
    role: Role,
    spec: &spg_engine::copy::CopyFromFileSpec,
    tx_state: &mut u8,
    tx_id: spg_engine::TxId,
) -> std::io::Result<()> {
    if role != Role::Admin {
        // PG: only superuser / pg_read_server_files may read server
        // files; SPG's admin is the superuser analog.
        send_error(
            stream,
            "42501",
            "permission denied to COPY from a file DETAIL: Only roles with privileges of the \
             \"pg_read_server_files\" role may COPY from a file.\nHINT:  Anyone can COPY to \
             stdout or from stdin. psql's \\copy command also works for anyone.",
        )?;
        return Ok(());
    }
    // PG's pre-file check order: relation / column existence /
    // duplicate column, all before the file is opened (probed r249).
    let target = match state
        .engine
        .read()
        .map_err(|_| std::io::Error::other("engine rwlock poisoned"))
        .map(|e| e.copy_target_columns(&spec.table, spec.columns.as_deref()))
    {
        Ok(Ok(t)) => t,
        Ok(Err(e)) => {
            let msg = format!("{e}");
            let code = if msg.contains("does not exist") && msg.contains("relation") && !msg.contains("column") {
                "42P01"
            } else if msg.contains("specified more than once") {
                "42701"
            } else {
                "42703"
            };
            send_error(stream, code, &msg)?;
            return Ok(());
        }
        Err(e) => return Err(e),
    };
    let data = match std::fs::read_to_string(&spec.path) {
        Ok(d) => d,
        Err(e) => {
            // PG's wording, without std's " (os error N)" suffix.
            let os = e.to_string();
            let os = os.split(" (os error").next().unwrap_or(&os);
            send_error(
                stream,
                "58P01",
                &format!(
                    "could not open file \"{path}\" for reading: {os}\nHINT:  COPY FROM \
                     instructs the PostgreSQL server process to read a file. You may want a \
                     client-side facility such as psql's \\copy.",
                    path = spec.path
                ),
            )?;
            return Ok(());
        }
    };
    let inserts = match spg_engine::copy::copy_buffer_inserts(
        &spec.table,
        spec.columns.as_deref(),
        &target,
        &spec.options,
        &data,
    ) {
        Ok(i) => i,
        Err(e) => {
            let msg = format!("{e}");
            let code = if msg.contains("missing data for column")
                || msg.contains("extra data after last expected column")
            {
                "22P04"
            } else {
                "22P02"
            };
            send_error(stream, code, &msg)?;
            return Ok(());
        }
    };
    // Same transaction + WAL discipline as the STDIN path (round 250).
    let wrap = !state.engine.read().is_ok_and(|e| e.is_tx_open(tx_id));
    let run = |state: &Arc<ServerState>, sql: &str| -> Result<(), String> {
        state
            .engine
            .write()
            .map_err(|_| "engine rwlock poisoned".to_string())
            .and_then(|mut e| e.execute_in(sql, tx_id).map(|_| ()).map_err(|err| format!("{err}")))
    };
    if wrap {
        if let Err(e) = run(state, "BEGIN") {
            send_error(stream, "XX000", &format!("COPY: {e}"))?;
            return Ok(());
        }
        if let Err(e) = crate::append_wal(state, "BEGIN", false) {
            let _ = run(state, "ROLLBACK");
            send_error(stream, "53100", &format!("{e}"))?;
            return Ok(());
        }
    }
    let mut inserted: u64 = 0;
    for insert in &inserts {
        let step = run(state, insert)
            .and_then(|()| crate::append_wal(state, insert, false).map_err(|e| format!("{e}")));
        match step {
            Ok(()) => inserted += 1,
            Err(msg) => {
                if wrap {
                    let _ = run(state, "ROLLBACK");
                    let _ = crate::append_wal(state, "ROLLBACK", false);
                }
                send_error(stream, "22P02", &msg)?;
                return Ok(());
            }
        }
    }
    if wrap {
        if let Err(e) = run(state, "COMMIT") {
            let _ = crate::append_wal(state, "ROLLBACK", false);
            send_error(stream, "XX000", &format!("COPY: {e}"))?;
            return Ok(());
        }
        if let Err(e) = crate::append_wal(state, "COMMIT", crate::session_sync_commit(state)) {
            send_error(stream, "53100", &format!("{e}"))?;
            return Ok(());
        }
    }
    send_command_complete(stream, &format!("COPY {inserted}"))?;
    *tx_state = if state.engine.read().is_ok_and(|e| e.is_tx_open(tx_id)) {
        b'T'
    } else {
        b'I'
    };
    Ok(())
}

fn handle_copy_from_stdin(
    stream: &mut dyn ReadWrite,
    state: &Arc<ServerState>,
    role: Role,
    table: &str,
    // v7.39 (read01 round 91) — the explicit `(col, …)` list, or None for the
    // whole-row form. Drives the per-row arity check and the INSERT mapping.
    column_list: Option<&[String]>,
    opts: &CopyOptions,
    tx_state: &mut u8,
    tx_id: spg_engine::TxId,
) -> std::io::Result<()> {
    if !role.can_write() {
        send_error(
            stream,
            "42501",
            "permission denied: COPY FROM requires admin or readwrite",
        )?;
        return Ok(());
    }
    // Look up the column count so we can size the CopyInResponse
    // and validate row arity.
    let table_col_names: Vec<String> = state
        .engine
        .read()
        .ok()
        .and_then(|e| {
            e.catalog()
                .get(table)
                .map(|t| t.schema().columns.iter().map(|c| c.name.clone()).collect())
        })
        .unwrap_or_default();
    // v7.39 (read01 round 91) — the columns each row must fill: the explicit
    // list when given, else the whole table. Used to name a missing column.
    let expected_names: Vec<String> = match column_list {
        Some(cols) => cols.to_vec(),
        None => table_col_names.clone(),
    };
    let Some(col_count) = state
        .engine
        .read()
        .ok()
        .and_then(|e| e.catalog().get(table).map(|t| t.schema().columns.len()))
    else {
        send_error(
            stream,
            "42P01",
            &format!("relation {table:?} does not exist"),
        )?;
        return Ok(());
    };
    // CopyInResponse 'G' body:
    //   [u8 overall_format = 0=text]
    //   [u16 col_count]
    //   per-col [u16 format = 0=text]
    let mut body = Vec::with_capacity(3 + col_count * 2);
    body.push(0);
    body.extend_from_slice(&u16::try_from(col_count).unwrap_or(0).to_be_bytes());
    for _ in 0..col_count {
        body.extend_from_slice(&0u16.to_be_bytes());
    }
    send_msg(stream, b'G', &body)?;

    // v7.39 (round 250) — COPY is ONE command: wrap the per-row INSERTs
    // in a transaction (engine AND WAL) unless the client already opened
    // one. Pre-r250 the rows went through `engine.execute` alone — never
    // WAL-appended (acknowledged rows vanished on kill -9: the r178/r180
    // lesson, wire-COPY spelling) and never rolled back on a bad row
    // (PG's COPY is all-or-nothing). A crash mid-COPY now replays
    // BEGIN + rows with no COMMIT, which the end-of-WAL auto-rollback
    // discards — exactly what the client observed (no success).
    // `ON_ERROR SET_NULL` (an SPG extension) is explicitly per-row:
    // a bad row must not poison the rest, but an error inside an engine
    // transaction aborts it (PG semantics) — so that mode keeps the
    // pre-r250 per-row autocommit shape (rows WAL-append per row, one
    // fsync at CommandComplete).
    // v7.39 (round 283) — ask about THIS connection's slot, not "is any
    // transaction open anywhere", which with one shared engine was every
    // other client's transaction too.
    let wrap = !opts.on_error_set_null
        && !state.engine.read().is_ok_and(|e| e.is_tx_open(tx_id));
    if wrap {
        if let Err(e) = state
            .engine
            .write()
            .map_err(|_| std::io::Error::other("engine rwlock poisoned"))
            .and_then(|mut e| {
                e.execute_in("BEGIN", tx_id)
                    .map(|_| ())
                    .map_err(|err| std::io::Error::other(format!("{err}")))
            })
        {
            send_error(stream, "XX000", &format!("COPY: {e}"))?;
            drain_copy_in_frames(stream)?;
            return Ok(());
        }
        if let Err(e) = crate::append_wal(state, "BEGIN", false) {
            let _ = state.engine.write().map(|mut en| en.execute_in("ROLLBACK", tx_id));
            send_error(stream, "53100", &format!("{e}"))?;
            drain_copy_in_frames(stream)?;
            return Ok(());
        }
    }
    // Roll the wrapping transaction back (engine + WAL) on any error
    // below, so the failed COPY leaves nothing — including on replay.
    let rollback_wrap = |state: &Arc<ServerState>| {
        if wrap {
            let _ = state.engine.write().map(|mut e| e.execute_in("ROLLBACK", tx_id));
            let _ = crate::append_wal(state, "ROLLBACK", false);
        }
    };

    // Stream loop: keep reading frames; each CopyData ('d') frame
    // may carry partial / multiple / no rows. Buffer bytes, split
    // on \n. CopyDone ('c') ends the input.
    let mut buf: Vec<u8> = Vec::new();
    let mut inserted: u64 = 0;
    let mut skipped: u64 = 0;
    loop {
        let mut header = [0u8; 5];
        stream.read_exact(&mut header)?;
        let ty = header[0];
        let len = u32::from_be_bytes([header[1], header[2], header[3], header[4]]) as usize;
        let body_len = len.saturating_sub(4);
        let mut body = vec![0u8; body_len];
        if body_len > 0 {
            stream.read_exact(&mut body)?;
        }
        match ty {
            b'd' => buf.extend_from_slice(&body),
            b'c' => {
                // Drain remaining bytes as a final row (if any).
                if !buf.is_empty() && !buf.ends_with(b"\n") {
                    buf.push(b'\n');
                }
                break;
            }
            b'f' => {
                send_error(stream, "57014", "client aborted COPY")?;
                return Ok(());
            }
            other => {
                send_error(
                    stream,
                    "08P01",
                    &format!("unexpected frame 0x{other:02x} during COPY"),
                )?;
                return Ok(());
            }
        }
        // Process whatever full lines we have.
        if let Err(msg) = process_copy_chunk(
            state,
            table,
            column_list,
            &expected_names,
            &mut buf,
            &mut inserted,
            &mut skipped,
            opts,
            tx_id,
        ) {
            let code = if msg.contains("missing data for column")
                || msg.contains("extra data after last expected column")
            {
                "22P04"
            } else {
                "22P02"
            };
            rollback_wrap(state);
            send_error(stream, code, &msg)?;
            drain_copy_in_frames(stream)?;
            return Ok(());
        }
    }
    // Final drain.
    if let Err(msg) = process_copy_chunk(
        state,
        table,
        column_list,
        &expected_names,
        &mut buf,
        &mut inserted,
        &mut skipped,
        opts,
        tx_id,
    ) {
        let code = if msg.contains("missing data for column")
            || msg.contains("extra data after last expected column")
        {
            "22P04"
        } else {
            "22P02"
        };
        rollback_wrap(state);
        send_error(stream, code, &msg)?;
        return Ok(());
    }
    // v7.39 (round 250) — the COMMIT is the durability point: one fsync
    // covers every row (session synchronous_commit honoured, like the
    // normal statement path).
    if !wrap && crate::session_sync_commit(state) {
        // Unwrapped path (ON_ERROR SET_NULL / client transaction): the
        // client-tx case fsyncs at its COMMIT; SET_NULL anchors here.
        if opts.on_error_set_null
            && inserted > 0
            && let Err(e) = crate::wal_fsync_now(state)
        {
            send_error(stream, "53100", &format!("{e}"))?;
            return Ok(());
        }
    }
    if wrap {
        if let Err(e) = state
            .engine
            .write()
            .map_err(|_| std::io::Error::other("engine rwlock poisoned"))
            .and_then(|mut e| {
                e.execute_in("COMMIT", tx_id)
                    .map(|_| ())
                    .map_err(|err| std::io::Error::other(format!("{err}")))
            })
        {
            let _ = crate::append_wal(state, "ROLLBACK", false);
            send_error(stream, "XX000", &format!("COPY: {e}"))?;
            return Ok(());
        }
        if let Err(e) = crate::append_wal(state, "COMMIT", crate::session_sync_commit(state)) {
            send_error(stream, "53100", &format!("{e}"))?;
            return Ok(());
        }
    }
    send_command_complete(stream, &format!("COPY {inserted}"))?;
    *tx_state = if state.engine.read().is_ok_and(|e| e.is_tx_open(tx_id)) {
        b'T'
    } else {
        b'I'
    };
    Ok(())
}

/// Split the buffer into newline-terminated rows, INSERT each one
/// via the regular engine path. Leftover bytes (partial row) stay
/// in `buf` for the next call.
fn process_copy_chunk(
    state: &Arc<ServerState>,
    table: &str,
    column_list: Option<&[String]>,
    expected_names: &[String],
    buf: &mut Vec<u8>,
    inserted: &mut u64,
    skipped: &mut u64,
    opts: &CopyOptions,
    // v7.39 (round 283) — the rows must land in the SAME slot as the
    // wrapping BEGIN/COMMIT. Routing them through `execute()` while the
    // transaction lived on the connection's slot meant COMMIT installed
    // the BEGIN-time shadow over them and every copied row vanished.
    tx_id: spg_engine::TxId,
) -> Result<(), String> {
    // FORMAT CSV: records are delimited by a newline that is NOT inside
    // a quoted field, so split with the quote-aware boundary scanner
    // rather than the plain `\n` split the text/JSON path uses.
    if opts.format_csv {
        let delim = opts.csv_delimiter.unwrap_or(',') as u8;
        let quote = opts.csv_quote.unwrap_or('"') as u8;
        while let Some(end) = spg_engine::copy::csv_record_end(buf, delim, quote) {
            let record: Vec<u8> = buf.drain(..end).collect();
            // Drop the terminating '\n' and an optional preceding '\r'.
            let mut rec = &record[..record.len() - 1];
            if rec.last() == Some(&b'\r') {
                rec = &rec[..rec.len() - 1];
            }
            if rec == b"\\." {
                return Ok(());
            }
            if rec.is_empty() {
                continue;
            }
            let row_text =
                std::str::from_utf8(rec).map_err(|_| "COPY row not valid UTF-8".to_string())?;
            if *skipped < opts.skip {
                *skipped += 1;
                continue;
            }
            let values = spg_engine::copy::decode_copy_csv_record(
                row_text,
                delim as char,
                quote as char,
                opts.null_string.as_deref().unwrap_or(""),
            );
            if let Err(msg) = copy_row_arity(&values, expected_names) {
                if opts.on_error_set_null {
                    continue;
                }
                return Err(msg);
            }
            let sql = spg_engine::copy::build_copy_insert(table, column_list, &values);
            {
                let mut engine = state
                    .engine
                    .write()
                    .map_err(|_| "engine rwlock poisoned".to_string())?;
                match engine.execute_in(&sql, tx_id) {
                    Ok(_) => *inserted += 1,
                    Err(e) => {
                        if opts.on_error_set_null {
                            continue;
                        }
                        // Bare engine error — PG reports the cell error
                        // itself, not an internal wrapper.
                        return Err(format!("{e}"));
                    }
                }
            }
            // v7.39 (round 250) — the row is in the wrapping transaction;
            // its WAL record must be too (no fsync — COMMIT covers).
            crate::append_wal(state, &sql, false).map_err(|e| format!("{e}"))?;
        }
        return Ok(());
    }
    while let Some(nl) = buf.iter().position(|&b| b == b'\n') {
        let line: Vec<u8> = buf.drain(..=nl).collect();
        let line = &line[..line.len() - 1]; // strip the '\n'
        // PG's COPY text format treats a single '.' on a line as
        // end-of-data (legacy psql). Honour it.
        if line == b"\\." {
            return Ok(());
        }
        if line.is_empty() {
            continue;
        }
        let row_text =
            std::str::from_utf8(line).map_err(|_| "COPY row not valid UTF-8".to_string())?;
        // v6.4.7 — SKIP N drops the first N data rows (typically a
        // CSV header row). `skipped` counts independently of
        // `inserted` so the final tag reports only successful
        // inserts.
        if *skipped < opts.skip {
            *skipped += 1;
            continue;
        }
        // v6.4.7 — FORMAT JSON decodes the line as a JSON object
        // and maps keys to column names. Default is the existing
        // tab-text format.
        let sql = if opts.format_json {
            match build_copy_insert_from_json(state, table, row_text, opts.on_error_set_null) {
                Ok(s) => s,
                Err(e) => {
                    if opts.on_error_set_null {
                        // Skip the bad row entirely under ON_ERROR.
                        continue;
                    }
                    return Err(format!("COPY FORMAT JSON: {e}"));
                }
            }
        } else {
            let values = decode_copy_text_row(row_text);
            if let Err(msg) = copy_row_arity(&values, expected_names) {
                if opts.on_error_set_null {
                    continue;
                }
                return Err(msg);
            }
            spg_engine::copy::build_copy_insert(table, column_list, &values)
        };
        {
            let mut engine = state
                .engine
                .write()
                .map_err(|_| "engine rwlock poisoned".to_string())?;
            match engine.execute_in(&sql, tx_id) {
                Ok(_) => *inserted += 1,
                Err(e) => {
                    if opts.on_error_set_null {
                        // Best-effort: skip the row but keep going.
                        continue;
                    }
                    // Bare engine error — PG reports the cell error
                    // itself, not an internal wrapper.
                    return Err(format!("{e}"));
                }
            }
        }
        // v7.39 (round 250) — WAL record inside the wrapping transaction.
        crate::append_wal(state, &sql, false).map_err(|e| format!("{e}"))?;
    }
    Ok(())
}

/// v6.4.7 — `FORMAT JSON`: decode the line as a JSON object, map
/// keys to the target table's column names (case-sensitive), and
/// build a positional INSERT.
fn build_copy_insert_from_json(
    state: &Arc<ServerState>,
    table: &str,
    line: &str,
    _on_error: bool,
) -> Result<String, String> {
    // Pull the column list from the catalog.
    let cols: Vec<String> = state
        .engine
        .read()
        .ok()
        .and_then(|e| {
            e.catalog()
                .get(table)
                .map(|t| t.schema().columns.iter().map(|c| c.name.clone()).collect())
        })
        .ok_or_else(|| format!("relation {table:?} does not exist"))?;
    // Hand-rolled minimal JSON-object parse: find each "key": value
    // pair at the top level. SPG's engine already has a JSON
    // parser, but pgwire.rs doesn't depend on spg-engine internals
    // for this path — we keep the parse local.
    let pairs = parse_json_object_top_level(line)?;
    let mut sql = format!("INSERT INTO {table} (");
    for (i, c) in cols.iter().enumerate() {
        if i > 0 {
            sql.push(',');
        }
        sql.push_str(c);
    }
    sql.push_str(") VALUES (");
    for (i, c) in cols.iter().enumerate() {
        if i > 0 {
            sql.push(',');
        }
        let val = pairs.iter().find(|(k, _)| k == c).map(|(_, v)| v.clone());
        match val {
            None => sql.push_str("NULL"),
            Some(v) => sql.push_str(&v),
        }
    }
    sql.push(')');
    Ok(sql)
}

/// Minimal top-level JSON object parser → Vec<(key, sql-literal)>.
/// Numbers / bool / null produce bare tokens; strings are
/// re-encoded as SQL single-quoted strings.
fn parse_json_object_top_level(s: &str) -> Result<Vec<(String, String)>, String> {
    let trimmed = s.trim();
    let body = trimmed
        .strip_prefix('{')
        .and_then(|s| s.strip_suffix('}'))
        .ok_or_else(|| "expected JSON object {...}".to_string())?;
    let mut out = Vec::new();
    let mut chars = body.chars().peekable();
    while chars.peek().is_some() {
        skip_ws(&mut chars);
        if chars.peek().is_none() {
            break;
        }
        let key = read_json_string(&mut chars)?;
        skip_ws(&mut chars);
        if chars.next() != Some(':') {
            return Err("expected ':' after key".into());
        }
        skip_ws(&mut chars);
        let val_sql = read_json_value_as_sql(&mut chars)?;
        out.push((key, val_sql));
        skip_ws(&mut chars);
        if chars.peek() == Some(&',') {
            chars.next();
        }
    }
    Ok(out)
}

fn skip_ws(chars: &mut std::iter::Peekable<std::str::Chars>) {
    while let Some(&c) = chars.peek() {
        if c.is_whitespace() {
            chars.next();
        } else {
            break;
        }
    }
}

fn read_json_string(chars: &mut std::iter::Peekable<std::str::Chars>) -> Result<String, String> {
    if chars.next() != Some('"') {
        return Err("expected '\"' to start string".into());
    }
    let mut out = String::new();
    loop {
        match chars.next() {
            None => return Err("unterminated JSON string".into()),
            Some('"') => return Ok(out),
            Some('\\') => {
                let n = chars.next().ok_or("trailing escape")?;
                out.push(match n {
                    '"' => '"',
                    '\\' => '\\',
                    '/' => '/',
                    'b' => '\u{08}',
                    'f' => '\u{0c}',
                    'n' => '\n',
                    'r' => '\r',
                    't' => '\t',
                    other => other,
                });
            }
            Some(c) => out.push(c),
        }
    }
}

fn read_json_value_as_sql(
    chars: &mut std::iter::Peekable<std::str::Chars>,
) -> Result<String, String> {
    skip_ws(chars);
    let Some(&first) = chars.peek() else {
        return Err("expected value".into());
    };
    match first {
        '"' => {
            let s = read_json_string(chars)?;
            // SQL-encode: escape single quotes.
            Ok(format!("'{}'", s.replace('\'', "''")))
        }
        't' | 'f' => {
            let mut s = String::new();
            while let Some(&c) = chars.peek() {
                if c.is_ascii_alphabetic() {
                    s.push(c);
                    chars.next();
                } else {
                    break;
                }
            }
            if s == "true" {
                Ok("TRUE".to_string())
            } else if s == "false" {
                Ok("FALSE".to_string())
            } else {
                Err(format!("invalid bool token: {s}"))
            }
        }
        'n' => {
            for expected in ['n', 'u', 'l', 'l'] {
                if chars.next() != Some(expected) {
                    return Err("invalid null token".into());
                }
            }
            Ok("NULL".to_string())
        }
        c if c == '-' || c.is_ascii_digit() => {
            let mut s = String::new();
            while let Some(&c) = chars.peek() {
                if c == '-' || c == '+' || c == '.' || c == 'e' || c == 'E' || c.is_ascii_digit() {
                    s.push(c);
                    chars.next();
                } else {
                    break;
                }
            }
            Ok(s)
        }
        other => Err(format!("unsupported JSON value start: {other:?}")),
    }
}

/// PG COPY text format: tab-separated cells, `\N` for NULL,
/// backslash escapes \\b \f \n \r \t \v. v7.22 — delegates to the
/// shared `spg_engine::copy` helper (single home with the embed
/// import path).
fn decode_copy_text_row(line: &str) -> Vec<Option<String>> {
    spg_engine::copy::decode_copy_text_row(line)
}

/// Build `INSERT INTO <table> VALUES (...)` from a decoded row.
/// v7.22 — delegates to `spg_engine::copy` (shared with the embed
/// import path; the stricter numeric check there also keeps
/// leading-zero codes ("0042") and float-ish words ("inf") quoted
/// instead of lossy bare literals). The wire path carries no
/// per-COPY column list (v7.15 scope note) — `None` emits the
/// positional form.
fn build_copy_insert(table: &str, values: &[Option<String>]) -> String {
    spg_engine::copy::build_copy_insert(table, None, values)
}

/// v7.39 (read01 round 91) — PG rejects a COPY row that does not carry exactly
/// one value per expected column: `missing data for column "X"` (naming the
/// first column left without data) for too few, `extra data after last expected
/// column` for too many. SPG used to feed a short row straight into an INSERT,
/// which quietly filled the trailing columns with NULL — silent data loss.
fn copy_row_arity(values: &[Option<String>], expected_names: &[String]) -> Result<(), String> {
    if values.len() < expected_names.len() {
        let missing = &expected_names[values.len()];
        return Err(format!("missing data for column \"{missing}\""));
    }
    if values.len() > expected_names.len() {
        return Err("extra data after last expected column".to_string());
    }
    Ok(())
}

/// COPY TO STDOUT — server runs `SELECT * FROM <table>`, sends
/// CopyOutResponse, streams each row as one CopyData frame (text
/// format), then CopyDone + CommandComplete.
fn handle_copy_to_stdout(
    stream: &mut dyn ReadWrite,
    state: &Arc<ServerState>,
    role: Role,
    sql: &str,
    opts: &CopyOptions,
    tx_state: &mut u8,
    tx_id: spg_engine::TxId,
) -> std::io::Result<()> {
    let _ = role.can_read(); // every role can read
    // v7.17.0 Phase 2.3 — COPY TO STDOUT does not honor
    // `statement_timeout` in this phase (the existing settings map
    // does not cross the COPY boundary); bulk-export paths get
    // `CancelToken::none()`. SPG_QUERY_TIMEOUT_MS / server-wide cap
    // still applies via the server-state watchdog.
    let result = execute_with_role(
        state,
        sql,
        role,
        CancelToken::none(),
        matches!(*tx_state, b'T' | b'E'),
        tx_id,
        &std::collections::HashMap::new(),
    );
    let (columns, rows) = match result {
        Ok(QueryResult::Rows { columns, rows }) => (columns, rows),
        Ok(QueryResult::CommandOk { .. }) => {
            send_error(stream, "42000", "COPY TO source produced no rows")?;
            return Ok(());
        }
        Err(e) => {
            send_error(stream, "42000", &e.to_string())?;
            return Ok(());
        }
        // v7.5.0 — QueryResult is #[non_exhaustive].
        Ok(_) => {
            send_error(stream, "XX000", "unexpected QueryResult variant")?;
            return Ok(());
        }
    };
    let col_count = columns.len();
    // CopyOutResponse 'H' body, same layout as CopyInResponse.
    let mut body = Vec::with_capacity(3 + col_count * 2);
    body.push(0);
    body.extend_from_slice(&u16::try_from(col_count).unwrap_or(0).to_be_bytes());
    for _ in 0..col_count {
        body.extend_from_slice(&0u16.to_be_bytes());
    }
    send_msg(stream, b'H', &body)?;
    let n = rows.len();
    let (wire_style, wire_tz) = state
        .engine
        .read()
        .map(|e| (e.render_style(), e.session_tz()))
        .unwrap_or((Default::default(), spg_engine::SessionTz::Utc));
    // v7.39 (read01 round 94) — honor WITH (FORMAT csv, HEADER, DELIMITER,
    // NULL). Format-specific escaping lives in the engine's copy encoders so
    // the TO path can't drift from the FROM path; the wire only builds the
    // per-cell Option<String> (None = SQL NULL) and picks text vs csv.
    let is_csv = opts.format_csv;
    let delimiter = opts
        .csv_delimiter
        .unwrap_or(if is_csv { ',' } else { '\t' });
    let quote = opts.csv_quote.unwrap_or('"');
    let null_str = opts.null_string.clone().unwrap_or_else(|| {
        if is_csv {
            String::new()
        } else {
            "\\N".to_string()
        }
    });
    let encode_line = |cells: &[Option<String>]| -> String {
        if is_csv {
            spg_engine::copy::encode_copy_csv_cells(cells, delimiter, quote, &null_str)
        } else {
            spg_engine::copy::encode_copy_text_cells_opts(cells, delimiter, &null_str)
        }
    };
    let mut send_line =
        |stream: &mut dyn ReadWrite, cells: &[Option<String>]| -> std::io::Result<()> {
            let mut line = encode_line(cells);
            line.push('\n');
            send_msg(stream, b'd', line.as_bytes())
        };
    if opts.header {
        let names: Vec<Option<String>> = columns.iter().map(|c| Some(c.name.clone())).collect();
        send_line(stream, &names)?;
    }
    for row in &rows {
        let cells: Vec<Option<String>> = row
            .values
            .iter()
            .enumerate()
            .map(|(i, v)| copy_cell_raw(v, columns.get(i).map(|c| c.ty), &wire_style, &wire_tz))
            .collect();
        send_line(stream, &cells)?;
    }
    send_msg(stream, b'c', &[])?; // CopyDone
    send_command_complete(stream, &format!("COPY {n}"))?;
    // No TX state change.
    let _ = tx_state;
    Ok(())
}

/// v7.39 (read01 round 94) — render a value as the RAW COPY cell text
/// (`None` for SQL NULL), BEFORE any format-specific escaping. The engine's
/// `encode_copy_{text,csv}_cells` apply the delimiter/quote/null escaping on
/// top, so keeping escaping out of here is what lets the same cell feed both
/// the text and csv encoders without double-escaping.
///
/// `ty` exists only to tell `timestamptz` from `timestamp`: PG's COPY renders
/// the former with its offset (`2024-01-15 10:30:00+00`), the latter without.
fn copy_cell_raw(
    v: &spg_storage::Value,
    ty: Option<spg_storage::DataType>,
    style: &spg_engine::eval::RenderStyle,
    tz: &spg_engine::SessionTz,
) -> Option<String> {
    use spg_storage::Value;
    let s = match v {
        Value::Null => return None,
        Value::Bool(b) => if *b { "t" } else { "f" }.to_string(),
        Value::SmallInt(n) => n.to_string(),
        Value::Int(n) => n.to_string(),
        Value::BigInt(n) => n.to_string(),
        Value::Float(x) => spg_engine::eval::format_float_styled(*x, style),
        Value::Real(x) => spg_engine::eval::format_real_styled(*x, style),
        Value::Text(s) | Value::Json(s) => s.to_string(),
        // v7.39 (bpchar epic) — COPY emits the padded stored form.
        Value::BpChar(s) => s.to_string(),
        // v7.39 (FTS) — canonical text forms for COPY too.
        Value::TsVector(lexs) => spg_engine::eval::format_tsvector(lexs),
        Value::TsQuery(ast) => spg_engine::eval::format_tsquery(ast),
        Value::Numeric {
            scaled,
            scale,
            kind,
        } => spg_engine::eval::format_numeric_kind(*kind, *scaled, *scale),
        Value::Date(d) => spg_engine::eval::format_date_styled(*d, style),
        Value::Timestamp(t) => {
            if matches!(ty, Some(DataType::Timestamptz)) {
                let abbr = tz.abbrev_at(*t);
                spg_engine::eval::format_timestamptz_tz(
                    *t,
                    style,
                    tz.offset_at(*t),
                    abbr.as_deref(),
                )
            } else {
                spg_engine::eval::format_timestamp_styled(*t, style)
            }
        }
        Value::Interval {
            months,
            days,
            micros,
        } => spg_engine::eval::format_interval_styled(*months, *days, *micros, style),
        Value::Vector(v) => {
            let parts: Vec<String> = v.iter().map(std::string::ToString::to_string).collect();
            format!("[{}]", parts.join(","))
        }
        // v6.0.1: COPY OUT a `VECTOR(N) USING SQ8` column — dequantise to f32
        // so the COPY text stream stays pgvector-compatible.
        Value::Sq8Vector(q) => {
            let parts: Vec<String> = spg_storage::quantize::dequantize(q)
                .iter()
                .map(std::string::ToString::to_string)
                .collect();
            format!("[{}]", parts.join(","))
        }
        // v6.0.3: COPY OUT for `VECTOR(N) USING HALF` — bit-exact dequantise.
        Value::HalfVector(h) => {
            let parts: Vec<String> = h
                .to_f32_vec()
                .iter()
                .map(std::string::ToString::to_string)
                .collect();
            format!("[{}]", parts.join(","))
        }
        // v7.5.0 — Value is #[non_exhaustive].
        other => spg_engine::eval::value_to_text(other),
    };
    Some(s)
}

// ---- Auth helpers (cleartext + SCRAM) ----

fn cleartext_auth(
    stream: &mut dyn ReadWrite,
    state: &Arc<ServerState>,
    user: &str,
) -> std::io::Result<Option<Role>> {
    send_msg(stream, b'R', &3u32.to_be_bytes())?;
    let pwd = read_password_message(stream)?;
    let verified = state
        .engine
        .read()
        .ok()
        .and_then(|e| e.verify_user(user, &pwd));
    if let Some(r) = verified {
        Ok(Some(r))
    } else {
        send_error(stream, "28P01", "password authentication failed")?;
        Ok(None)
    }
}

/// v4.8 SCRAM-SHA-256 server-side flow. Returns `Some(role)` on
/// successful proof verification, `None` if anything goes wrong
/// (caller closes the connection — error frame already written).
fn scram_auth(
    stream: &mut dyn ReadWrite,
    state: &Arc<ServerState>,
    user: &str,
    secure: bool,
) -> std::io::Result<Option<Role>> {
    // ---- Step 1: AuthenticationSASL ----
    // Mechanism list is null-terminated mechanism strings, ended by
    // an empty string (= double null). Over TLS with a SHA-256 cert we
    // additionally advertise SCRAM-SHA-256-PLUS and bind to
    // `tls-server-end-point` (RFC 5929).
    let cbind_hash = if secure {
        crate::mysqlwire::tls_channel_binding_hash()
    } else {
        None
    };
    let advertise_plus = cbind_hash.is_some();
    let mut sasl_body = Vec::new();
    sasl_body.extend_from_slice(&10u32.to_be_bytes());
    if advertise_plus {
        // PLUS listed first — clients pick the strongest offered.
        sasl_body.extend_from_slice(b"SCRAM-SHA-256-PLUS\0SCRAM-SHA-256\0\0");
    } else {
        sasl_body.extend_from_slice(b"SCRAM-SHA-256\0\0");
    }
    send_msg(stream, b'R', &sasl_body)?;

    // ---- Step 2: read SASLInitialResponse ('p') ----
    let mut header = [0u8; 5];
    stream.read_exact(&mut header)?;
    if header[0] != b'p' {
        send_error(stream, "28000", "expected SASLInitialResponse")?;
        return Ok(None);
    }
    let len = u32::from_be_bytes([header[1], header[2], header[3], header[4]]) as usize;
    let mut body = vec![0u8; len.saturating_sub(4)];
    stream.read_exact(&mut body)?;
    // Body: [mech_name \0][i32 client_first_len][client_first bytes]
    let Some(mech_end) = body.iter().position(|&b| b == 0) else {
        send_error(
            stream,
            "28000",
            "SASLInitial: mechanism name not null-terminated",
        )?;
        return Ok(None);
    };
    let mech = std::str::from_utf8(&body[..mech_end]).unwrap_or("");
    let using_plus = match mech {
        "SCRAM-SHA-256-PLUS" => {
            if !advertise_plus {
                send_error(
                    stream,
                    "28000",
                    "SCRAM-SHA-256-PLUS not available on this connection",
                )?;
                return Ok(None);
            }
            true
        }
        "SCRAM-SHA-256" => false,
        _ => {
            send_error(
                stream,
                "28000",
                &format!("only SCRAM-SHA-256[-PLUS] is supported, got {mech:?}"),
            )?;
            return Ok(None);
        }
    };
    let mut cur = mech_end + 1;
    if cur + 4 > body.len() {
        send_error(stream, "28000", "SASLInitial: missing client-first length")?;
        return Ok(None);
    }
    let cf_len =
        u32::from_be_bytes([body[cur], body[cur + 1], body[cur + 2], body[cur + 3]]) as usize;
    cur += 4;
    if cur + cf_len > body.len() {
        send_error(stream, "28000", "SASLInitial: client-first truncated")?;
        return Ok(None);
    }
    let Ok(client_first_msg) = std::str::from_utf8(&body[cur..cur + cf_len]).map(str::to_string)
    else {
        send_error(stream, "28000", "SASLInitial: client-first not UTF-8")?;
        return Ok(None);
    };
    let client_first = match crate::scram::parse_client_first(&client_first_msg) {
        Ok(c) => c,
        Err(e) => {
            send_error(stream, "28000", &e.to_string())?;
            return Ok(None);
        }
    };

    // ---- Step 2b: GS2 flag / mechanism consistency + downgrade guard ----
    // The gs2-cbind-flag must agree with the chosen mechanism, and a client
    // that flags `y` (channel-binding-capable but "server didn't offer it")
    // while we *did* advertise -PLUS signals a stripped-advertisement MITM.
    use crate::scram::Gs2CbindFlag;
    match (&client_first.cbind_flag, using_plus) {
        (Gs2CbindFlag::Required, true) => {}
        (Gs2CbindFlag::Required, false) => {
            send_error(
                stream,
                "28000",
                "SCRAM: channel-binding flag set on a non-PLUS mechanism",
            )?;
            return Ok(None);
        }
        (Gs2CbindFlag::NotSupported | Gs2CbindFlag::SupportedNotUsed, true) => {
            send_error(
                stream,
                "28000",
                "SCRAM-SHA-256-PLUS requires the p=tls-server-end-point flag",
            )?;
            return Ok(None);
        }
        (Gs2CbindFlag::SupportedNotUsed, false) => {
            if advertise_plus {
                send_error(stream, "28000", "SCRAM: channel-binding downgrade detected")?;
                return Ok(None);
            }
        }
        (Gs2CbindFlag::NotSupported, false) => {}
    }
    // The cbind-data bound into the client-final `c=` value: the cert hash for
    // a -PLUS exchange, empty otherwise.
    let cbind_data: &[u8] = if using_plus {
        cbind_hash.as_ref().map(|h| h.as_slice()).unwrap_or(&[])
    } else {
        &[]
    };

    // ---- Step 3: pull this user's SCRAM secrets ----
    let secrets = state
        .engine
        .read()
        .ok()
        .and_then(|e| {
            e.users()
                .iter()
                .find(|(n, _)| *n == user)
                .map(|(_, r)| r.scram().cloned())
        })
        .flatten();
    let Some(secrets) = secrets else {
        send_error(stream, "28P01", "user has no SCRAM verifier on file")?;
        return Ok(None);
    };

    // ---- Step 4: server-first ----
    let server_nonce = match random_nonce_b64(18) {
        Ok(n) => n,
        Err(e) => {
            send_error(stream, "58000", &format!("RNG failure: {e}"))?;
            return Ok(None);
        }
    };
    let combined_nonce = format!("{}{}", client_first.client_nonce, server_nonce);
    let server_first = crate::scram::build_server_first(&combined_nonce, &secrets);
    let mut cont_body = Vec::new();
    cont_body.extend_from_slice(&11u32.to_be_bytes());
    cont_body.extend_from_slice(server_first.as_bytes());
    send_msg(stream, b'R', &cont_body)?;

    // ---- Step 5: read SASLResponse with client-final ----
    let mut header = [0u8; 5];
    stream.read_exact(&mut header)?;
    if header[0] != b'p' {
        send_error(stream, "28000", "expected SASLResponse")?;
        return Ok(None);
    }
    let len = u32::from_be_bytes([header[1], header[2], header[3], header[4]]) as usize;
    let mut body = vec![0u8; len.saturating_sub(4)];
    stream.read_exact(&mut body)?;
    let Ok(client_final_msg) = std::str::from_utf8(&body).map(str::to_string) else {
        send_error(stream, "28000", "SASLResponse: client-final not UTF-8")?;
        return Ok(None);
    };
    let client_final = match crate::scram::parse_client_final(&client_final_msg) {
        Ok(f) => f,
        Err(e) => {
            send_error(stream, "28000", &e.to_string())?;
            return Ok(None);
        }
    };
    // Validate the channel binding the client echoed in `c=`: it must equal
    // base64(gs2-header || cbind-data). For -PLUS this is what actually binds
    // the auth to *this* TLS cert — a MITM presenting a different cert makes
    // the client's cbind-data (and hence c=) mismatch what we compute here.
    let expected_c = crate::scram::channel_binding_c_value(&client_first.gs2_header, cbind_data);
    if client_final.channel_binding != expected_c {
        send_error(stream, "28000", "SCRAM: channel binding mismatch")?;
        return Ok(None);
    }
    if client_final.combined_nonce != combined_nonce {
        send_error(stream, "28000", "SCRAM: nonce mismatch")?;
        return Ok(None);
    }

    // ---- Step 6: verify proof, send SASLFinal + AuthOk ----
    let server_signature = match crate::scram::verify_and_sign(
        &secrets,
        &client_first.bare,
        &server_first,
        &client_final.without_proof,
        &client_final.client_proof,
    ) {
        Ok(s) => s,
        Err(e) => {
            send_error(stream, "28P01", &e.to_string())?;
            return Ok(None);
        }
    };
    let mut final_body = Vec::new();
    final_body.extend_from_slice(&12u32.to_be_bytes());
    final_body.extend_from_slice(server_signature.as_bytes());
    send_msg(stream, b'R', &final_body)?;

    // Role: the user table lookup we did earlier was for scram secrets;
    // re-read for the role specifically.
    let role = state.engine.read().ok().and_then(|e| {
        e.users()
            .iter()
            .find(|(n, _)| *n == user)
            .map(|(_, r)| r.role)
    });
    Ok(role)
}

/// 18 random bytes → ~24 base64 chars, used as the SCRAM server
/// nonce. Sourced from /dev/urandom — same RNG path the v4.8
/// user-record salt comes from.
fn random_nonce_b64(byte_len: usize) -> std::io::Result<String> {
    let mut buf = vec![0u8; byte_len];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut buf)?;
    Ok(spg_crypto::base64::encode(&buf))
}

// ---- Startup message ----

fn read_startup(stream: &mut dyn ReadWrite) -> std::io::Result<(String, Vec<(String, String)>)> {
    // First u32 (BE) is total length. Read it, then the rest.
    loop {
        let mut len_bytes = [0u8; 4];
        stream.read_exact(&mut len_bytes)?;
        let total = u32::from_be_bytes(len_bytes) as usize;
        if total < 8 {
            return Err(std::io::Error::other("startup message too short"));
        }
        let mut body = vec![0u8; total - 4];
        stream.read_exact(&mut body)?;
        let proto = u32::from_be_bytes([body[0], body[1], body[2], body[3]]);
        // SSLRequest (proto = 80877103) → reply 'N' to refuse SSL and
        // expect a real startup next.
        if proto == 80877103 {
            stream.write_all(b"N")?;
            continue;
        }
        // GSSENCRequest (80877104) → same treatment.
        if proto == 80877104 {
            stream.write_all(b"N")?;
            continue;
        }
        if proto != PROTOCOL_V3 {
            return Err(std::io::Error::other(format!(
                "unsupported protocol version: {proto}"
            )));
        }
        // Rest is a sequence of null-terminated key/value strings,
        // terminated by an empty key.
        let mut params = Vec::new();
        let mut user = String::new();
        let mut p = 4;
        while p < body.len() {
            let k_end = body[p..]
                .iter()
                .position(|&b| b == 0)
                .ok_or_else(|| std::io::Error::other("startup key not null-terminated"))?;
            let key = std::str::from_utf8(&body[p..p + k_end])
                .map_err(|_| std::io::Error::other("startup key not UTF-8"))?
                .to_string();
            p += k_end + 1;
            if key.is_empty() {
                break;
            }
            let v_end = body[p..]
                .iter()
                .position(|&b| b == 0)
                .ok_or_else(|| std::io::Error::other("startup value not null-terminated"))?;
            let value = std::str::from_utf8(&body[p..p + v_end])
                .map_err(|_| std::io::Error::other("startup value not UTF-8"))?
                .to_string();
            p += v_end + 1;
            if key == "user" {
                user = value.clone();
            }
            params.push((key, value));
        }
        return Ok((user, params));
    }
}

fn read_password_message(stream: &mut dyn ReadWrite) -> std::io::Result<String> {
    let mut header = [0u8; 5];
    stream.read_exact(&mut header)?;
    if header[0] != b'p' {
        return Err(std::io::Error::other("expected PasswordMessage"));
    }
    let len = u32::from_be_bytes([header[1], header[2], header[3], header[4]]) as usize;
    let body_len = len.saturating_sub(4);
    let mut body = vec![0u8; body_len];
    stream.read_exact(&mut body)?;
    let pw = body.strip_suffix(b"\0").unwrap_or(&body);
    std::str::from_utf8(pw)
        .map(str::to_string)
        .map_err(|_| std::io::Error::other("password not UTF-8"))
}

// ---- Message writers ----

fn send_msg(stream: &mut dyn Write, ty: u8, body: &[u8]) -> std::io::Result<()> {
    let len = u32::try_from(body.len() + 4)
        .map_err(|_| std::io::Error::other("PG message body too large"))?;
    let mut out = Vec::with_capacity(5 + body.len());
    out.push(ty);
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(body);
    stream.write_all(&out)
}

fn send_parameter_status(stream: &mut dyn Write, key: &str, value: &str) -> std::io::Result<()> {
    let mut body = Vec::with_capacity(key.len() + value.len() + 2);
    body.extend_from_slice(key.as_bytes());
    body.push(0);
    body.extend_from_slice(value.as_bytes());
    body.push(0);
    send_msg(stream, b'S', &body)
}

fn send_ready_for_query(stream: &mut dyn Write, state: u8) -> std::io::Result<()> {
    send_msg(stream, b'Z', &[state])
}

/// v7.37.x (SPGS PLUCK 红线 — hand-rolled `SELECT <int>` response).
/// Builds the full PG wire response for a literal-int SELECT
/// (RowDescription + DataRow + CommandComplete + ReadyForQuery)
/// straight into `out` with zero `Vec` / `String` / `ColumnSchema` /
/// `Row` / `format!` intermediates. The literal-int SELECT path is
/// hit per liveness probe / pool keepalive / canary canary so even
/// small per-query allocations show up on the wire-probe (~5 µs
/// each = 50 µs / 10-alloc legacy path), and PG 18's response time
/// for this shape is ~21 µs end-to-end on mini — every byte spent
/// on bookkeeping closes a wire-probe gap.
fn encode_select_int_response(out: &mut Vec<u8>, n: i64, tx_state: u8) -> std::io::Result<()> {
    // RowDescription frame body: 1 field, name "?column?", type
    // BigInt (OID 20, size 8, modifier -1), text format (0).
    //   [2 bytes: nfields=1]
    //   [name="?column?\0" (9 bytes)]
    //   [4 bytes: table OID (0)]
    //   [2 bytes: column attr num (0)]
    //   [4 bytes: type OID (20)]
    //   [2 bytes: type size (8)]
    //   [4 bytes: type modifier (-1)]
    //   [2 bytes: format code (0 = text)]
    // body length = 2 + 9 + 4 + 2 + 4 + 2 + 4 + 2 = 29
    // frame total = 1 byte (op 'T') + 4 byte len + 29 body = 34
    //
    // v7.39 (round 320, V53) — TWO frames, picked by the literal's width.
    // PG types an integer literal that fits in int4 as `integer` and only
    // a wider one as `bigint` (`SELECT pg_typeof(1), pg_typeof(2147483648)`
    // → `integer|bigint`). This path baked OID 20 into the bytes, so the
    // single most-run query on the wire — `SELECT 1` — described itself as
    // int8: a driver that maps by OID handed the application a 64-bit
    // value where PG (and SPG's own engine) give a 32-bit one.
    #[rustfmt::skip]
    const ROW_DESC_INT8: [u8; 34] = [
        b'T',
        0, 0, 0, 33,         // length = 4 + 29
        0, 1,                 // nfields = 1
        b'?', b'c', b'o', b'l', b'u', b'm', b'n', b'?', 0, // name
        0, 0, 0, 0,           // table OID
        0, 0,                 // column attr num
        0, 0, 0, 20,          // type OID = BigInt
        0, 8,                 // type size = 8
        255, 255, 255, 255,   // type modifier = -1
        0, 0,                 // format code = text
    ];
    #[rustfmt::skip]
    const ROW_DESC_INT4: [u8; 34] = [
        b'T',
        0, 0, 0, 33,
        0, 1,
        b'?', b'c', b'o', b'l', b'u', b'm', b'n', b'?', 0,
        0, 0, 0, 0,
        0, 0,
        0, 0, 0, 23,          // type OID = Int
        0, 4,                 // type size = 4
        255, 255, 255, 255,
        0, 0,
    ];
    out.extend_from_slice(if i32::try_from(n).is_ok() {
        &ROW_DESC_INT4
    } else {
        &ROW_DESC_INT8
    });

    // DataRow frame: 1 cell, the decimal text of `n`.
    //   [2 bytes: nfields=1]
    //   [4 bytes: cell length]
    //   [cell text bytes]
    // Encode the int decimal text into a small scratch buffer first
    // so we know its length.
    let mut digits = [0u8; 24];
    let mut pos = digits.len();
    let (mut x, negative) = if n < 0 {
        ((n as i128).unsigned_abs() as u64, true)
    } else {
        (n as u64, false)
    };
    loop {
        pos -= 1;
        digits[pos] = b'0' + (x % 10) as u8;
        x /= 10;
        if x == 0 {
            break;
        }
    }
    if negative {
        pos -= 1;
        digits[pos] = b'-';
    }
    let int_text = &digits[pos..];
    let cell_len = int_text.len() as u32;
    let frame_len = 4 + 2 + 4 + cell_len; // length-field + nfields + cell_len_prefix + cell
    out.push(b'D');
    out.extend_from_slice(&frame_len.to_be_bytes());
    out.extend_from_slice(&1u16.to_be_bytes()); // nfields = 1
    out.extend_from_slice(&cell_len.to_be_bytes());
    out.extend_from_slice(int_text);

    // CommandComplete: "SELECT 1\0" (tag is row count, always 1).
    //   length = 4 + 9 = 13; total = 1 + 13 = 14
    #[rustfmt::skip]
    const COMPLETE_FRAME: [u8; 14] = [
        b'C',
        0, 0, 0, 13,
        b'S', b'E', b'L', b'E', b'C', b'T', b' ', b'1', 0,
    ];
    out.extend_from_slice(&COMPLETE_FRAME);

    // ReadyForQuery: length 5, state byte.
    //   total = 1 + 5 = 6
    out.push(b'Z');
    out.extend_from_slice(&5u32.to_be_bytes());
    out.push(tx_state);
    Ok(())
}

fn send_command_complete(stream: &mut dyn Write, tag: &str) -> std::io::Result<()> {
    let mut body = Vec::with_capacity(tag.len() + 1);
    body.extend_from_slice(tag.as_bytes());
    body.push(0);
    send_msg(stream, b'C', &body)
}

/// v7.37.42-arena Phase 5 fallback — inline `SELECT <n>` command-tag
/// encoder writing the wire frame directly into `wbuf`. Skips the
/// `format!()` heap alloc + the `send_msg` Vec<u8> intermediate
/// (saves ~100-200 ns per call on the SCALARSQ hot path, paid back at
/// the cumulative attack budget). The 'C' message body is just the
/// null-terminated ASCII tag, so emit `SELECT ` + the integer's
/// decimal digits + `\0` in place after the 1+4 byte frame header.
fn send_command_complete_select_count(out: &mut Vec<u8>, n: usize) -> std::io::Result<()> {
    // Encode the integer's decimal text into a small scratch buffer.
    // u64 max = 20 digits, but `n: usize` on a 64-bit host fits 20;
    // pre-allocate 24 to be safe across pointer widths.
    let mut digits = [0u8; 24];
    let mut pos = digits.len();
    let mut x = n as u64;
    loop {
        pos -= 1;
        digits[pos] = b'0' + (x % 10) as u8;
        x /= 10;
        if x == 0 {
            break;
        }
    }
    let int_text = &digits[pos..];
    // Body = "SELECT " (7 bytes) + int_text + NUL (1 byte).
    // Frame length field = 4 + body_len.
    let body_len = 7 + int_text.len() + 1;
    let frame_len = u32::try_from(4 + body_len)
        .map_err(|_| std::io::Error::other("PG message body too large"))?;
    out.reserve(1 + 4 + body_len);
    out.push(b'C');
    out.extend_from_slice(&frame_len.to_be_bytes());
    out.extend_from_slice(b"SELECT ");
    out.extend_from_slice(int_text);
    out.push(0);
    Ok(())
}

/// v7.39 (read01 round 46) — drain the NOTICEs the statement just executed
/// raised and send one NoticeResponse each, ahead of the row / command reply
/// (PG's order). Called on every simple-query path, unconditionally, so a
/// failed statement can't leak its notices into the next one.
fn drain_notices(state: &ServerState, wbuf: &mut Vec<u8>) -> std::io::Result<()> {
    let notices = match state.engine.write() {
        Ok(mut e) => e.take_notices(),
        Err(_) => return Ok(()),
    };
    for n in &notices {
        send_notice(wbuf, n.severity, &n.message)?;
    }
    // v7.39 (round 318, V51) — diagnostics raised by the host side of a
    // builtin (pg_cancel_backend / pg_terminate_backend naming an id that
    // is not a live connection). The engine has no registry, so the
    // warning is produced where the fact is known and drained here.
    for w in crate::take_host_warnings() {
        send_notice(wbuf, spg_engine::NoticeSeverity::Warning, &w)?;
    }
    Ok(())
}

/// v7.39 (round 222) — LISTEN/NOTIFY wire delivery. Two phases at every
/// statement boundary: (1) drain the engine's committed-notification queue
/// and BROADCAST each entry to every registered connection's per-conn
/// queue — whichever connection reaches a boundary first performs the
/// broadcast, so a NOTIFY from connection B reaches connection A's queue
/// even before A runs anything; (2) flush THIS connection's queue as 'A'
/// NotificationResponse messages ([i32 pid][cstr channel][cstr payload]).
/// libpq / psycopg2 pick them up on their next interaction.
fn drain_notifications(
    state: &ServerState,
    wbuf: &mut Vec<u8>,
    conn_state: &crate::ConnState,
) -> std::io::Result<()> {
    let notifies = match state.engine.write() {
        Ok(mut e) => e.take_notifications(),
        Err(_) => return Ok(()),
    };
    if !notifies.is_empty()
        && let Ok(conns) = state.connections.read()
    {
        for c in conns.iter() {
            if let Ok(mut q) = c.notify_queue.lock() {
                q.extend(notifies.iter().cloned());
            }
        }
    }
    let mine: Vec<(String, String)> = match conn_state.notify_queue.lock() {
        Ok(mut q) => core::mem::take(&mut *q),
        Err(_) => Vec::new(),
    };
    for (channel, payload) in &mine {
        let mut body = Vec::with_capacity(channel.len() + payload.len() + 8);
        body.extend_from_slice(&conn_state.pid.to_be_bytes());
        body.extend_from_slice(channel.as_bytes());
        body.push(0);
        body.extend_from_slice(payload.as_bytes());
        body.push(0);
        send_msg(wbuf, b'A', &body)?;
    }
    Ok(())
}

/// v7.39 (read01 round 46) — NoticeResponse ('N'). Same field encoding as
/// ErrorResponse but severity NOTICE and SQLSTATE 00000
/// (successful_completion), which is what PG sends for the `IF EXISTS` /
/// `IF NOT EXISTS` "…, skipping" notices. `V` carries the non-localized
/// severity PG has emitted since 9.6; libpq/psql print `NOTICE:  <msg>`.
fn send_notice(
    stream: &mut dyn Write,
    severity: spg_engine::NoticeSeverity,
    msg: &str,
) -> std::io::Result<()> {
    // v7.39 (round 318, V41) — the severity comes from the engine now.
    // It used to be hardcoded NOTICE, so a PG WARNING arrived as a
    // NOTICE and the drivers that surface warnings but drop notices
    // silently swallowed it. SQLSTATE stays `00000` for both: PG's
    // warnings on this channel carry successful_completion, and the
    // command did succeed.
    let mut body = Vec::new();
    body.push(b'S');
    body.extend_from_slice(severity.as_pg_str().as_bytes());
    body.push(0);
    body.push(b'V');
    body.extend_from_slice(severity.as_pg_str().as_bytes());
    body.push(0);
    body.push(b'C');
    body.extend_from_slice(b"00000");
    body.push(0);
    body.push(b'M');
    body.extend_from_slice(msg.as_bytes());
    body.push(0);
    body.push(0);
    stream.write_all(b"N")?;
    stream.write_all(
        &u32::try_from(body.len() + 4)
            .unwrap_or(u32::MAX)
            .to_be_bytes(),
    )?;
    stream.write_all(&body)
}

/// v7.39 (round 318, V51) — has this connection been terminated?
fn terminated(conn_state: &crate::ConnState) -> bool {
    conn_state
        .terminate
        .load(std::sync::atomic::Ordering::Relaxed)
}

/// v7.39 (round 318, V51) — PG's reply to the backend it just terminated:
/// severity FATAL (not ERROR — the connection is going away), SQLSTATE
/// `57P01` admin_shutdown, and the exact text PG 18.4 emits.
fn send_fatal_terminated(stream: &mut dyn Write) -> std::io::Result<()> {
    let mut body = Vec::new();
    for (code, val) in [
        (b'S', "FATAL"),
        (b'V', "FATAL"),
        (b'C', "57P01"),
        (b'M', "terminating connection due to administrator command"),
    ] {
        body.push(code);
        body.extend_from_slice(val.as_bytes());
        body.push(0);
    }
    body.push(0);
    stream.write_all(b"E")?;
    stream.write_all(
        &u32::try_from(body.len() + 4)
            .unwrap_or(u32::MAX)
            .to_be_bytes(),
    )?;
    stream.write_all(&body)
}

fn send_error(stream: &mut dyn Write, sqlstate: &str, msg: &str) -> std::io::Result<()> {
    send_error_pos(stream, sqlstate, msg, None)
}

/// v7.39 (read01 round 95) — `send_error` plus PG's `P` (error position)
/// field: a 1-based character offset into the query the client sent. psql
/// renders it as the `LINE n: … ^` caret. `None` omits the field (the common
/// case — only syntax errors currently carry a position).
fn send_error_pos(
    stream: &mut dyn Write,
    sqlstate: &str,
    msg: &str,
    position: Option<usize>,
) -> std::io::Result<()> {
    // ErrorResponse: each field is `[fieldcode byte][value][\0]`,
    // terminated by a single `\0`. Base set: S (severity), C
    // (sqlstate), M (message). v7.39 (SQLSTATE fidelity) — when the
    // engine message carries PG's constraint phrasing, lift the
    // structured PG_DIAG fields ORMs branch on: n (constraint name),
    // t (table name), D (detail), s (schema — SPG is single-schema
    // `public`).
    let mut body = Vec::new();
    body.push(b'S');
    body.extend_from_slice(b"ERROR");
    body.push(0);
    body.push(b'C');
    body.extend_from_slice(sqlstate.as_bytes());
    body.push(0);
    // v7.39 (GUC knife 5) — split a trailing "\nHINT:  ..." into its
    // own H field, like PG (the DateStyle input hint rides this).
    let (msg, hint) = match msg.split_once("\nHINT:  ") {
        Some((m, h)) => (m, Some(h)),
        None => (msg, None),
    };
    // Split a trailing " DETAIL: ..." into its own D field, like PG.
    let (main, detail) = match msg.split_once(" DETAIL: ") {
        Some((m, d)) => (m, Some(d)),
        None => (msg, None),
    };
    // v7.39 — engine constraint errors ride EngineError::Unsupported,
    // whose Display prefixes "unsupported: ". That prefix is only
    // truthful for the generic 42000 class; a typed SQLSTATE means we
    // understood the error precisely, so strip it from the client
    // message (PG has no such prefix).
    let main_msg: &str = if sqlstate != "42000" {
        crate::strip_internal_error_prefixes(main)
    } else {
        main
    };
    // PG's 23505 message carries no table suffix — the table lands in
    // the PG_DIAG `t` field (extracted from `main` below, which keeps
    // the suffix). ORMs regex the PG message shape exactly.
    let main_msg: &str = match (sqlstate, main_msg.find(" on table \"")) {
        // v7.39 (round 210) — 23P01 EXCLUDE carries the same engine-side
        // ` on table "…"` suffix; PG's exclusion message has none either.
        ("23505" | "23P01", Some(cut)) => &main_msg[..cut],
        _ => main_msg,
    };
    body.push(b'M');
    body.extend_from_slice(main_msg.as_bytes());
    body.push(0);
    if let Some(d) = detail {
        body.push(b'D');
        body.extend_from_slice(d.as_bytes());
        body.push(0);
    }
    if let Some(h) = hint {
        body.push(b'H');
        body.extend_from_slice(h.as_bytes());
        body.push(0);
    }
    let quoted_after = |marker: &str| -> Option<&str> {
        let rest = &main[main.find(marker)? + marker.len()..];
        rest.strip_prefix('"')?.split('"').next()
    };
    if let Some(con) = quoted_after("violates unique constraint ")
        .or_else(|| quoted_after("violates foreign key constraint "))
        .or_else(|| quoted_after("violates check constraint "))
        .or_else(|| quoted_after("violates exclusion constraint "))
    {
        body.push(b'n');
        body.extend_from_slice(con.as_bytes());
        body.push(0);
    }
    if let Some(t) = quoted_after("on table ").or_else(|| quoted_after("of relation ")) {
        body.push(b't');
        body.extend_from_slice(t.as_bytes());
        body.push(0);
        body.push(b's');
        body.extend_from_slice(b"public");
        body.push(0);
    }
    if let Some(c) = quoted_after("null value in column ") {
        body.push(b'c');
        body.extend_from_slice(c.as_bytes());
        body.push(0);
    }
    // v7.39 (read01 round 95) — the P (position) field, when the error carries
    // one. PG numbers characters from 1; psql draws the `LINE n: … ^` caret at
    // this offset.
    if let Some(p) = position {
        body.push(b'P');
        body.extend_from_slice(p.to_string().as_bytes());
        body.push(0);
    }
    body.push(0);
    send_msg(stream, b'E', &body)
}

fn send_row_description(stream: &mut dyn Write, cols: &[ColumnSchema]) -> std::io::Result<()> {
    let body = encode_row_description_body(cols);
    send_msg(stream, b'T', &body)
}

/// v7.37 — write `b'T'` + length + body straight into `out` from
/// pre-encoded RowDescription bytes cached on `PreparedStmt`.
/// Used by the extended-query Execute path; skips the per-Execute
/// `encode_row_description_body` round.
fn send_row_description_cached(out: &mut Vec<u8>, body: &[u8]) -> std::io::Result<()> {
    send_msg(out, b'T', body)
}

fn encode_row_description_body(cols: &[ColumnSchema]) -> Vec<u8> {
    let n = u16::try_from(cols.len()).unwrap_or(u16::MAX);
    let mut body = Vec::with_capacity(2 + cols.len() * 24);
    body.extend_from_slice(&n.to_be_bytes());
    for c in cols {
        body.extend_from_slice(c.name.as_bytes());
        body.push(0);
        body.extend_from_slice(&0u32.to_be_bytes()); // table OID (unknown)
        body.extend_from_slice(&0u16.to_be_bytes()); // attribute number
        body.extend_from_slice(&pg_type_oid(c.ty).to_be_bytes()); // type OID
        body.extend_from_slice(&pg_type_len(c.ty).to_be_bytes()); // type len (i16)
        body.extend_from_slice(&(-1i32).to_be_bytes()); // type modifier
        body.extend_from_slice(&0u16.to_be_bytes()); // format = text
    }
    body
}

/// v7.39 (binary results) — PG binary-format epoch shift: dates and
/// timestamps are relative to 2000-01-01 on the wire, SPG stores them
/// Unix-relative.
const PG_EPOCH_DAYS: i32 = 10_957;
const PG_EPOCH_MICROS: i64 = 946_684_800_000_000;

/// v7.39 (binary results) — encode one cell in PG's binary send
/// format: `[i32 len][payload]`, NULL = len -1. Types without a
/// binary encoder yet return an error (an honest protocol failure
/// beats bytes the client will mis-decode).
fn encode_binary_cell(out: &mut Vec<u8>, v: &Value, ty: DataType) -> Result<(), String> {
    let mut put = |payload: &[u8]| {
        out.extend_from_slice(&(payload.len() as i32).to_be_bytes());
        out.extend_from_slice(payload);
    };
    match v {
        Value::Null => out.extend_from_slice(&(-1i32).to_be_bytes()),
        Value::Bool(b) => put(&[u8::from(*b)]),
        Value::SmallInt(n) => put(&n.to_be_bytes()),
        Value::Int(n) => put(&n.to_be_bytes()),
        Value::BigInt(n) => put(&n.to_be_bytes()),
        Value::Real(x) => put(&x.to_be_bytes()),
        Value::Float(x) => put(&x.to_be_bytes()),
        // Text-family binary format IS the UTF-8 payload.
        Value::Text(s) | Value::BpChar(s) => put(s.as_bytes()),
        Value::Json(s) => {
            if matches!(ty, DataType::Jsonb) {
                // jsonb binary: 1-byte version tag then the text.
                let mut buf = Vec::with_capacity(s.len() + 1);
                buf.push(1);
                buf.extend_from_slice(s.as_bytes());
                put(&buf);
            } else {
                put(s.as_bytes());
            }
        }
        Value::Xml(s) => put(s.as_bytes()),
        Value::Bytes(b) => put(b),
        Value::Uuid(u) => put(&u[..]),
        Value::Date(days) => put(&(days - PG_EPOCH_DAYS).to_be_bytes()),
        Value::Timestamp(us) => put(&(us - PG_EPOCH_MICROS).to_be_bytes()),
        Value::Time(us) => put(&us.to_be_bytes()),
        Value::Interval {
            months,
            days,
            micros,
        } => {
            let mut buf = Vec::with_capacity(16);
            buf.extend_from_slice(&micros.to_be_bytes());
            buf.extend_from_slice(&days.to_be_bytes());
            buf.extend_from_slice(&months.to_be_bytes());
            put(&buf);
        }
        Value::Numeric { scaled, scale, .. } => {
            put(&numeric_binary(*scaled, *scale));
        }
        Value::NumericBig(b) => {
            // Route through the decimal string; big numerics are rare
            // on hot paths and the digits fit the same wire shape.
            let (scaled, scale) = decimal_str_to_scaled(&b.to_decimal_str())
                .ok_or("binary numeric: value out of range")?;
            put(&numeric_binary(scaled, scale));
        }
        // v7.39 — 1-D array binary format: ndim, hasnull, element
        // OID, (len, lower-bound=1), then per-element [len][payload].
        Value::IntArray(items) => put(&binary_array(items, 23, |v, b| {
            b.extend_from_slice(&v.to_be_bytes());
        })),
        Value::BigIntArray(items) => put(&binary_array(items, 20, |v, b| {
            b.extend_from_slice(&v.to_be_bytes());
        })),
        Value::SmallIntArray(items) => put(&binary_array(items, 21, |v, b| {
            b.extend_from_slice(&v.to_be_bytes());
        })),
        Value::FloatArray(items) => put(&binary_array(items, 701, |v, b| {
            b.extend_from_slice(&v.to_be_bytes());
        })),
        Value::BoolArray(items) => put(&binary_array(items, 16, |v, b| {
            b.push(u8::from(*v));
        })),
        Value::TextArray(items) => put(&binary_array(items, 25, |v, b| {
            b.extend_from_slice(v.as_bytes());
        })),
        Value::UuidArray(items) => put(&binary_array(items, 2950, |v, b| {
            b.extend_from_slice(&v[..]);
        })),
        other => {
            return Err(format!(
                "binary result format not implemented for {:?}",
                other.data_type()
            ));
        }
    }
    Ok(())
}

/// v7.39 — encode a 1-D array in PG's binary array format. `enc`
/// writes one element's payload; NULL elements get len -1.
fn binary_array<T>(items: &[Option<T>], elem_oid: u32, enc: impl Fn(&T, &mut Vec<u8>)) -> Vec<u8> {
    let has_null = items.iter().any(Option::is_none);
    let mut buf = Vec::with_capacity(20 + items.len() * 8);
    buf.extend_from_slice(&1i32.to_be_bytes()); // ndim
    buf.extend_from_slice(&i32::from(has_null).to_be_bytes());
    buf.extend_from_slice(&elem_oid.to_be_bytes());
    buf.extend_from_slice(&(items.len() as i32).to_be_bytes());
    buf.extend_from_slice(&1i32.to_be_bytes()); // lower bound
    for it in items {
        match it {
            None => buf.extend_from_slice(&(-1i32).to_be_bytes()),
            Some(v) => {
                let mut payload = Vec::new();
                enc(v, &mut payload);
                buf.extend_from_slice(&(payload.len() as i32).to_be_bytes());
                buf.extend_from_slice(&payload);
            }
        }
    }
    buf
}

/// Parse a plain decimal string into (scaled i128, scale) — the big
/// numeric bridge for the binary encoder.
fn decimal_str_to_scaled(s: &str) -> Option<(i128, u16)> {
    let (int_part, frac_part) = match s.split_once('.') {
        Some((i, f)) => (i, f),
        None => (s, ""),
    };
    let digits: String = format!("{int_part}{frac_part}");
    let scaled: i128 = digits.parse().ok()?;
    Some((scaled, u16::try_from(frac_part.len()).ok()?))
}

/// v7.39 — PG `numeric` binary send format: base-10000 digit groups
/// with a weight (exponent of the first group), sign word and
/// display scale. Matches numeric_send in behaviour for finite
/// values (SPG's Numeric kind NaN/Inf never reach the wire — they
/// format as text through float paths).
fn numeric_binary(scaled: i128, scale: u16) -> Vec<u8> {
    let neg = scaled < 0;
    let mut abs = scaled.unsigned_abs();
    // Left-pad the fractional side to a whole number of 4-digit
    // groups so group boundaries align with base-10000 positions.
    let scale_usize = scale as usize;
    let frac_pad = (4 - (scale_usize % 4)) % 4;
    for _ in 0..frac_pad {
        abs *= 10;
    }
    let frac_groups = (scale_usize + frac_pad) / 4;
    // Collect base-10000 digits, least significant first.
    let mut groups: Vec<u16> = Vec::new();
    if abs == 0 {
        groups.push(0);
    }
    while abs > 0 {
        groups.push((abs % 10_000) as u16);
        abs /= 10_000;
    }
    while groups.len() <= frac_groups {
        groups.push(0); // ensure at least one integer-part group
    }
    // Trim trailing zero groups on the fractional end and leading
    // zero groups on the integer end (PG sends a minimal digit list).
    let mut lo = 0;
    while lo < frac_groups && groups[lo] == 0 {
        lo += 1;
    }
    let mut hi = groups.len();
    while hi > frac_groups + 1 && groups[hi - 1] == 0 {
        hi -= 1;
    }
    let digits: Vec<u16> = groups[lo..hi].iter().rev().copied().collect();
    // weight = base-10000 exponent of the FIRST sent digit.
    let weight = (hi - frac_groups) as i32 - 1;
    let all_zero = digits.iter().all(|&d| d == 0);
    let (digits, weight) = if all_zero {
        (Vec::new(), 0)
    } else {
        (digits, weight)
    };
    let mut buf = Vec::with_capacity(8 + digits.len() * 2);
    buf.extend_from_slice(&(digits.len() as i16).to_be_bytes());
    buf.extend_from_slice(&(weight as i16).to_be_bytes());
    buf.extend_from_slice(&(if neg { 0x4000u16 } else { 0 }).to_be_bytes());
    buf.extend_from_slice(&scale.to_be_bytes());
    for d in &digits {
        buf.extend_from_slice(&d.to_be_bytes());
    }
    buf
}

/// v7.39 (binary results) — DataRow with per-column format codes:
/// binary columns ride encode_binary_cell, text columns the arena
/// text path. Only used when the Bind requested any binary column.
fn encode_data_row_formats(
    out: &mut Vec<u8>,
    cols: &[ColumnSchema],
    row: &Row,
    formats: &[i16],
    arena: &bumpalo::Bump,
    style: &spg_engine::eval::RenderStyle,
    tz: &spg_engine::SessionTz,
) -> std::io::Result<()> {
    let mut body: Vec<u8> = Vec::with_capacity(cols.len() * 12);
    body.extend_from_slice(&(cols.len() as u16).to_be_bytes());
    for (i, c) in cols.iter().enumerate() {
        let v = row.values.get(i).unwrap_or(&Value::Null);
        if col_is_binary(formats, i) {
            encode_binary_cell(&mut body, v, c.ty).map_err(std::io::Error::other)?;
        } else {
            match value_to_pg_text(v, Some(c.ty), arena, style, tz) {
                None => body.extend_from_slice(&(-1i32).to_be_bytes()),
                Some(s) => {
                    body.extend_from_slice(&(s.len() as i32).to_be_bytes());
                    body.extend_from_slice(s.as_bytes());
                }
            }
        }
    }
    send_msg(out, b'D', &body)
}

fn send_data_row(stream: &mut dyn Write, cols: &[ColumnSchema], row: &Row) -> std::io::Result<()> {
    // v7.37.42-arena Phase 3 — per-call arena for the cell text-format
    // fallback path. `send_data_row` covers canned/catalog responses
    // and a handful of extended-protocol slow-path rows (line 1408 +
    // legacy fallbacks), all very low row volume — arena lifetime
    // scoped to this single row is fine.
    let arena = bumpalo::Bump::new();
    let n = u16::try_from(row.values.len())
        .map_err(|_| std::io::Error::other("DataRow: too many cells"))?;
    let mut body = Vec::with_capacity(2 + row.values.len() * 8);
    body.extend_from_slice(&n.to_be_bytes());
    // Canned/catalog rows carry pre-rendered text cells; the default
    // render style is correct (and no engine lock is available here).
    let style = spg_engine::eval::RenderStyle::default();
    let tz = spg_engine::SessionTz::Utc;
    for (i, v) in row.values.iter().enumerate() {
        encode_pg_text_cell(&mut body, v, cols.get(i).map(|c| c.ty), &arena, &style, &tz)?;
    }
    send_msg(stream, b'D', &body)
}

/// v7.37 — borrowed-cells variant of `encode_data_row`. Same wire
/// format, but the row is presented as `&[&Value]` (cell references
/// out of the source rows) instead of a fully owned `&Row`. The
/// streaming SELECT path uses this to skip the per-row `.cloned()`
/// the materialising path pays before reaching the wire.
fn encode_data_row_from_refs(
    out: &mut Vec<u8>,
    cols: &[ColumnSchema],
    values: &[&spg_storage::Value<'_>],
    arena: &bumpalo::Bump,
    style: &spg_engine::eval::RenderStyle,
    tz: &spg_engine::SessionTz,
) -> std::io::Result<()> {
    let n = u16::try_from(values.len())
        .map_err(|_| std::io::Error::other("DataRow: too many cells"))?;
    let frame_start = out.len();
    out.push(b'D');
    out.extend_from_slice(&[0u8; 4]); // length placeholder, backpatched below
    out.extend_from_slice(&n.to_be_bytes());
    for (i, v) in values.iter().enumerate() {
        encode_pg_text_cell(out, v, cols.get(i).map(|c| c.ty), arena, style, tz)?;
    }
    let body_plus_len_field = out.len() - frame_start - 1;
    let len = u32::try_from(body_plus_len_field)
        .map_err(|_| std::io::Error::other("PG message body too large"))?;
    out[frame_start + 1..frame_start + 5].copy_from_slice(&len.to_be_bytes());
    Ok(())
}

/// v7.34 (SPGS perf bar) — direct-into-`Vec` DataRow encoder, used by
/// the simple-query Q hot path. `send_data_row` above always built two
/// intermediate `Vec`s per row (one for the body, one stitched together
/// inside `send_msg`) and copied between them, so a 25 k-row PROJ
/// burned 50 k `Vec::with_capacity` calls + 25 k body→frame copies
/// before the bytes even reached `wbuf`. This variant writes the
/// `b'D'` frame straight into the simple-query response buffer with a
/// backpatched length prefix — zero intermediate allocations per row.
/// `send_data_row` is kept around for canned responses and the
/// extended-protocol path (line 1625), which see at most a handful of
/// rows per call and don't justify the same buffer-handle threading.
fn encode_data_row(
    out: &mut Vec<u8>,
    cols: &[ColumnSchema],
    row: &Row,
    arena: &bumpalo::Bump,
    style: &spg_engine::eval::RenderStyle,
    tz: &spg_engine::SessionTz,
) -> std::io::Result<()> {
    encode_data_row_from_values(out, cols, &row.values, arena, style, tz)
}

/// v7.37.42-arena Phase 2 — `&[Value]` variant of `encode_data_row`.
/// The SCALARSQ streaming executor keeps an arena-backed per-row
/// `bumpalo::Vec<Value>` scratch and hands a borrowed slice to the
/// wire encoder without wrapping in a fresh `Row` (and the
/// `Vec<Value>` alloc it would force on the 100-row-per-query hot
/// path). The cells themselves are encoded identically to
/// `encode_data_row`.
fn encode_data_row_from_values(
    out: &mut Vec<u8>,
    cols: &[ColumnSchema],
    values: &[Value<'_>],
    arena: &bumpalo::Bump,
    style: &spg_engine::eval::RenderStyle,
    tz: &spg_engine::SessionTz,
) -> std::io::Result<()> {
    let n = u16::try_from(values.len())
        .map_err(|_| std::io::Error::other("DataRow: too many cells"))?;
    let frame_start = out.len();
    out.push(b'D');
    out.extend_from_slice(&[0u8; 4]); // length placeholder, backpatched below
    out.extend_from_slice(&n.to_be_bytes());
    for (i, v) in values.iter().enumerate() {
        encode_pg_text_cell(out, v, cols.get(i).map(|c| c.ty), arena, style, tz)?;
    }
    let body_plus_len_field = out.len() - frame_start - 1;
    let len = u32::try_from(body_plus_len_field)
        .map_err(|_| std::io::Error::other("PG message body too large"))?;
    out[frame_start + 1..frame_start + 5].copy_from_slice(&len.to_be_bytes());
    Ok(())
}

/// v7.34 (B5 ledger) — type-dispatched text-mode cell encoder for the
/// hot projection types. The legacy `value_to_pg_text` always returns
/// an owned `String` (per-cell `to_string()` + `s.clone()`), which on
/// a 25 k-row × 5-column projection burns 125 k heap allocations + 125 k
/// `Vec::extend_from_slice` copies. The fast paths below write straight
/// into the DataRow body with no intermediate `String`:
///   * BIGINT/INT/SMALLINT — std `Display` on a 24-byte stack buffer.
///   * BOOL — fixed `t`/`f` byte slice.
///   * TEXT/JSON — borrow the existing buffer (no clone).
/// Less common types (Float / Numeric / Timestamp / Vector / Uuid /
/// arrays / hstore / range) fall back to `value_to_pg_text`; they're
/// rare on row-projection-heavy workloads.
fn encode_pg_text_cell(
    out: &mut Vec<u8>,
    v: &Value<'_>,
    ty: Option<DataType>,
    arena: &bumpalo::Bump,
    style: &spg_engine::eval::RenderStyle,
    tz: &spg_engine::SessionTz,
) -> std::io::Result<()> {
    match v {
        Value::Null => {
            out.extend_from_slice(&(-1i32).to_be_bytes());
            return Ok(());
        }
        Value::Bool(b) => return write_cell_bytes(out, if *b { b"t" } else { b"f" }),
        Value::SmallInt(n) => return write_cell_int(out, i64::from(*n)),
        Value::Int(n) => return write_cell_int(out, i64::from(*n)),
        Value::BigInt(n) => return write_cell_int(out, *n),
        Value::Text(s) | Value::Json(s) => return write_cell_bytes(out, s.as_bytes()),
        // v7.39 (bpchar epic) — padded stored form on the wire.
        Value::BpChar(s) => return write_cell_bytes(out, s.as_bytes()),
        // v7.34.6 — Timestamp / Date / Timestamptz fast paths. The
        // mailrs `proj_25k` baseline emits one `internal_date`
        // Timestamp per row × 25 k rows = 25 k `format_timestamp`
        // calls, each going through `format!()` → owned `String` →
        // `write_cell_bytes` copy. `write_cell_timestamp` below
        // writes the ISO-8601 chars (with optional `+00` suffix for
        // Timestamptz) straight into the DataRow body via a 32-byte
        // stack scratch, with no intermediate `String`. Output
        // matches `spg_engine::eval::format_timestamp` byte-for-byte
        // (incl. the `frac` trailing-zero trim) — see
        // `write_cell_timestamp_matches_engine_format` below.
        // v7.39 (GUC knife 3) — the stack fast paths spell ISO; a
        // non-ISO DateStyle drops to the styled formatter below.
        // v7.39 (GUC knife 6, BC) — so does a BC value (astronomical
        // year <= 0, i.e. any day before 0001-01-01 = day -719162).
        Value::Timestamp(micros)
            if style.date_style == spg_engine::eval::DateStyleKind::Iso
                && *micros >= AD_FLOOR_DAYS * 86_400_000_000
                && *micros != i64::MAX
                && (tz.is_utc() || !matches!(ty, Some(DataType::Timestamptz))) =>
        {
            let with_tz = matches!(ty, Some(DataType::Timestamptz));
            return write_cell_timestamp(out, *micros, with_tz);
        }
        Value::Date(days)
            if style.date_style == spg_engine::eval::DateStyleKind::Iso
                && i64::from(*days) >= AD_FLOOR_DAYS
                && *days != i32::MAX =>
        {
            return write_cell_date(out, *days);
        }
        _ => {}
    }
    match value_to_pg_text(v, ty, arena, style, tz) {
        None => out.extend_from_slice(&(-1i32).to_be_bytes()),
        Some(s) => write_cell_bytes(out, s.as_bytes())?,
    }
    Ok(())
}

fn write_cell_bytes(out: &mut Vec<u8>, bytes: &[u8]) -> std::io::Result<()> {
    let len =
        i32::try_from(bytes.len()).map_err(|_| std::io::Error::other("cell value too large"))?;
    // v7.35.0 — single `reserve` + single `copy_from_nonoverlapping`
    // burst, replacing the two `extend_from_slice` calls. Each
    // `extend_from_slice` runs its own capacity check + `memmove`
    // dispatch, so a 25 k-row × 5-cell PROJ paid 250 k extra
    // capacity branches + 125 k extra memmove invocations vs the
    // contiguous layout below. `reserve` makes the bound check
    // exact-once; the manual ptr writes copy length-prefix + payload
    // back-to-back; `set_len` advances the head past both.
    let total = 4usize + bytes.len();
    out.reserve(total);
    let len_be = len.to_be_bytes();
    // SAFETY: `reserve` above guarantees `out.capacity() >=
    // out.len() + total`; the writes stay within `[head, head+total)`
    // which is in bounds and uninitialised. `set_len` after both
    // writes finalises the new length.
    #[allow(unsafe_code)]
    unsafe {
        let head = out.as_mut_ptr().add(out.len());
        core::ptr::copy_nonoverlapping(len_be.as_ptr(), head, 4);
        core::ptr::copy_nonoverlapping(bytes.as_ptr(), head.add(4), bytes.len());
        out.set_len(out.len() + total);
    }
    Ok(())
}

#[inline]
fn write_pad2(buf: &mut [u8], p: &mut usize, n: u32) {
    buf[*p] = b'0' + ((n / 10) % 10) as u8;
    buf[*p + 1] = b'0' + (n % 10) as u8;
    *p += 2;
}

#[inline]
fn write_pad4(buf: &mut [u8], p: &mut usize, n: u32) {
    buf[*p] = b'0' + ((n / 1000) % 10) as u8;
    buf[*p + 1] = b'0' + ((n / 100) % 10) as u8;
    buf[*p + 2] = b'0' + ((n / 10) % 10) as u8;
    buf[*p + 3] = b'0' + (n % 10) as u8;
    *p += 4;
}

#[inline]
fn write_pad6(buf: &mut [u8], p: &mut usize, n: u32) {
    buf[*p] = b'0' + ((n / 100_000) % 10) as u8;
    buf[*p + 1] = b'0' + ((n / 10_000) % 10) as u8;
    buf[*p + 2] = b'0' + ((n / 1_000) % 10) as u8;
    buf[*p + 3] = b'0' + ((n / 100) % 10) as u8;
    buf[*p + 4] = b'0' + ((n / 10) % 10) as u8;
    buf[*p + 5] = b'0' + (n % 10) as u8;
    *p += 6;
}

// v7.34.6 — direct-into-`Vec` Timestamp encoder, matching
// `spg_engine::eval::format_timestamp` byte-for-byte (including the
// trailing-zero trim on the fractional component). Bails to the
// engine formatter for out-of-range years (< 0 or > 9999) — those
// hit pg_dump regression corpora, not the per-row PROJ hot path,
// so the slow path is fine there.
/// v7.39 (GUC knife 6, BC) — days-since-epoch of 0001-01-01; anything
/// below is a BC value the stack fast paths don't spell (they have no
/// " BC" suffix logic) — those cells fall back to the engine formatter.
const AD_FLOOR_DAYS: i64 = -719_162;

fn write_cell_timestamp(out: &mut Vec<u8>, micros: i64, with_tz: bool) -> std::io::Result<()> {
    const MICROS_PER_DAY: i64 = 86_400_000_000;
    let days = micros.div_euclid(MICROS_PER_DAY);
    let day_micros = micros.rem_euclid(MICROS_PER_DAY);
    let secs = day_micros / 1_000_000;
    let frac = (day_micros % 1_000_000) as u32;
    let (y, m, d, _, _, _) = secs_to_ymdhms(days * 86_400);
    if !(0..=9999).contains(&y) {
        let s = if with_tz {
            spg_engine::eval::format_timestamptz(micros)
        } else {
            spg_engine::eval::format_timestamp(micros)
        };
        return write_cell_bytes(out, s.as_bytes());
    }
    let hh = (secs / 3600) as u32;
    let mm = ((secs / 60) % 60) as u32;
    let ss = (secs % 60) as u32;
    let mut buf = [0u8; 32];
    let mut p = 0;
    write_pad4(&mut buf, &mut p, y as u32);
    buf[p] = b'-';
    p += 1;
    write_pad2(&mut buf, &mut p, m);
    buf[p] = b'-';
    p += 1;
    write_pad2(&mut buf, &mut p, d);
    buf[p] = b' ';
    p += 1;
    write_pad2(&mut buf, &mut p, hh);
    buf[p] = b':';
    p += 1;
    write_pad2(&mut buf, &mut p, mm);
    buf[p] = b':';
    p += 1;
    write_pad2(&mut buf, &mut p, ss);
    if frac != 0 {
        buf[p] = b'.';
        p += 1;
        let frac_start = p;
        write_pad6(&mut buf, &mut p, frac);
        // Match eval::format_timestamp's trailing-zero trim.
        while p > frac_start && buf[p - 1] == b'0' {
            p -= 1;
        }
    }
    if with_tz {
        buf[p] = b'+';
        p += 1;
        buf[p] = b'0';
        p += 1;
        buf[p] = b'0';
        p += 1;
    }
    write_cell_bytes(out, &buf[..p])
}

// v7.34.6 — direct-into-`Vec` Date encoder, matching the
// `format_date` helper below byte-for-byte.
fn write_cell_date(out: &mut Vec<u8>, days: i32) -> std::io::Result<()> {
    let secs = i64::from(days) * 86_400;
    let (y, m, d, _, _, _) = secs_to_ymdhms(secs);
    if !(0..=9999).contains(&y) {
        return write_cell_bytes(out, format_date(days).as_bytes());
    }
    let mut buf = [0u8; 10];
    let mut p = 0;
    write_pad4(&mut buf, &mut p, y as u32);
    buf[p] = b'-';
    p += 1;
    write_pad2(&mut buf, &mut p, m);
    buf[p] = b'-';
    p += 1;
    write_pad2(&mut buf, &mut p, d);
    write_cell_bytes(out, &buf[..p])
}

fn write_cell_int(out: &mut Vec<u8>, n: i64) -> std::io::Result<()> {
    // i64::MIN is "-9223372036854775808" — 20 chars + sign. The buffer
    // below leaves headroom so the loop's `pos -= 1` never underflows
    // even on the worst case.
    let mut buf = [0u8; 24];
    let mut pos = buf.len();
    let (mut x, negative) = if n < 0 {
        // Negate via u64 so i64::MIN doesn't overflow.
        ((n as i128).unsigned_abs() as u64, true)
    } else {
        (n as u64, false)
    };
    // ASCII decimal, least-significant digit first into the tail of the
    // buffer; std::fmt::Write's Display machinery costs measurably more
    // per call (state machine + formatter setup) than a tight loop
    // here, and at 50 k BigInt cells per 25 k-row PROJ that adds up.
    loop {
        pos -= 1;
        buf[pos] = b'0' + (x % 10) as u8;
        x /= 10;
        if x == 0 {
            break;
        }
    }
    if negative {
        pos -= 1;
        buf[pos] = b'-';
    }
    write_cell_bytes(out, &buf[pos..])
}

// ---- Type mapping ----

/// The stable PG wire-protocol type OIDs (16 = bool, 23 = int4, …). These
/// numbers are part of the on-the-wire interface a PG client decodes by,
/// so SPG must emit the same values to stay drop-in compatible — the same
/// interface constants every PG-compatible server / driver conforms to.
/// Catch-all is `text` (25) so an unknown / new SPG type round-trips
/// as a readable string rather than confusing the client.
const fn pg_type_oid(ty: DataType) -> u32 {
    match ty {
        DataType::Bool => 16,
        // v7.39 (round 291) — PG's identifier type carries its own OID;
        // reporting text (25) here would make every catalog column
        // introspect as text, which is what ORMs key off.
        DataType::Name => 19,
        DataType::SmallInt => 21,
        DataType::Int => 23,
        DataType::BigInt => 20,
        DataType::Float => 701,
        DataType::Real => 700,
        DataType::Text | DataType::Varchar(_) | DataType::Char(_) | DataType::Vector { .. } => 25,
        DataType::Timestamp => 1114,
        DataType::Timestamptz => 1184, // v7.9.2 mailrs blocker fix
        DataType::Date => 1082,
        DataType::Interval => 1186,
        DataType::Numeric { .. } => 1700,
        DataType::Json => 114,         // PG `json`
        DataType::Jsonb => 3802,       // PG `jsonb` — v7.9.0 mailrs blocker fix
        DataType::Bytes => 17,         // PG `bytea` — v7.10.4 Epic 1
        DataType::TextArray => 1009,   // PG `_text` (TEXT[]) — v7.10.9 Epic 2
        DataType::IntArray => 1007,    // PG `_int4` (INT[]) — v7.11.12 Epic 3
        DataType::BigIntArray => 1016, // PG `_int8` (BIGINT[]) — v7.11.12 Epic 3
        DataType::TsVector => 3614,    // PG `tsvector` — v7.12.0 G-CRIT-3
        DataType::TsQuery => 3615,     // PG `tsquery` — v7.12.0 G-CRIT-3
        DataType::Uuid => 2950,        // PG `uuid` — v7.17.0 Phase 3 P0-25
        DataType::Time => 1083,        // PG `time` — v7.17.0 Phase 3 P0-32
        // v7.17.0 Phase 3 P0-33 — MySQL YEAR has no dedicated PG
        // OID; advertise as INT4 (23) so libpq / sqlx render it
        // as an integer.
        DataType::Year => 23,
        // v7.17.0 Phase 3 P0-34 — PG TIMETZ OID 1266.
        DataType::TimeTz => 1266,
        // v7.17.0 Phase 3 P0-35 — PG MONEY OID 790.
        DataType::Money => 790,
        // v7.17.0 Phase 3 P0-38 — PG range OIDs (pg_type.dat).
        DataType::Range(k) => match k {
            spg_storage::RangeKind::Int4 => 3904,
            spg_storage::RangeKind::Int8 => 3926,
            spg_storage::RangeKind::Num => 3906,
            spg_storage::RangeKind::Ts => 3908,
            spg_storage::RangeKind::TsTz => 3910,
            spg_storage::RangeKind::Date => 3912,
        },
        // v7.17.0 Phase 3 P0-39 — hstore OID is installation-
        // dependent in real PG. Advertise as TEXT (25) on the
        // wire so clients without an installed hstore extension
        // still decode the canonical `"k"=>"v"` text correctly.
        DataType::Hstore => 25,
        // v7.17.0 Phase 3 P0-40 — 2D arrays reuse the 1D OIDs
        // (PG carries dimension count in the array data header,
        // not the OID).
        DataType::IntArray2D => 1007,
        DataType::BigIntArray2D => 1016,
        DataType::TextArray2D => 1009,
        // v7.39 (read01 round 75) — `_bool` (1000): PG reports the SAME oid for an
        // array however many dimensions it has.
        DataType::BoolArray2D => 1000,
        // v7.37.5 β-P4 — PG `_interval` (INTERVAL[]) OID 1187.
        DataType::IntervalArray => 1187,
        // v7.37.5 γ — array-of-scalar family OIDs (pg_type.dat).
        DataType::BoolArray => 1000,        // _bool
        DataType::SmallIntArray => 1005,    // _int2
        DataType::FloatArray => 1022,       // _float8
        DataType::NumericArray => 1231,     // _numeric
        DataType::DateArray => 1182,        // _date
        DataType::TimestampArray => 1115,   // _timestamp
        DataType::TimestamptzArray => 1185, // _timestamptz
        DataType::UuidArray => 2951,        // _uuid
        DataType::JsonArray => 199,         // _json
        DataType::JsonbArray => 3807,       // _jsonb
        DataType::BytesArray => 1001,       // _bytea
        DataType::VarcharArray => 1015,     // _varchar
        DataType::CharArray => 1014,        // _bpchar
        // v7.37.5 ε — PG geometry OIDs (pg_type.dat).
        DataType::Point => 600,
        DataType::Lseg => 601,
        DataType::Path => 602,
        DataType::PgBox => 603,
        DataType::Polygon => 604,
        DataType::Line => 628,
        DataType::Circle => 718,
        // v7.37.5 ζ-A — network / bit / xml / "char" / money[].
        DataType::Inet => 869,
        DataType::Cidr => 650,
        DataType::Macaddr => 829,
        DataType::Macaddr8 => 774,
        DataType::PgLsn => 3220,
        DataType::Bit(_) => 1560,
        DataType::BitVarying(_) => 1562,
        DataType::Xml => 142,
        DataType::Char1 => 18,
        DataType::MoneyArray => 791,
        // v7.37.5 δ — PG 14+ multirange OIDs (pg_type.dat).
        DataType::Multirange(k) => match k {
            spg_storage::RangeKind::Int4 => 4451,
            spg_storage::RangeKind::Int8 => 4537,
            spg_storage::RangeKind::Num => 4536,
            spg_storage::RangeKind::Ts => 4533,
            spg_storage::RangeKind::TsTz => 4534,
            spg_storage::RangeKind::Date => 4535,
        },
    }
}

const fn pg_type_len(ty: DataType) -> i16 {
    match ty {
        DataType::Bool => 1,
        DataType::SmallInt => 2,
        // v7.38 (T-tstz Phase 1) — float4 is 4 bytes; it fell to the varlena
        // catch-all while already carrying the fixed-length OID 700.
        DataType::Int | DataType::Date | DataType::Real => 4,
        // v7.38 (T-tstz Phase 1) — timestamptz is a fixed 8-byte type (PG
        // typlen 8), same as timestamp; it fell to the varlena catch-all.
        DataType::BigInt | DataType::Float | DataType::Timestamp | DataType::Timestamptz => 8,
        DataType::Interval => 16,
        // v7.17.0 — UUID is fixed 16 bytes (RFC 4122 / PG OID 2950).
        DataType::Uuid => 16,
        // v7.17.0 Phase 3.P0-32 — TIME is fixed i64 (8 bytes).
        DataType::Time => 8,
        // v7.17.0 Phase 3.P0-33 — YEAR is fixed u16 (2 bytes).
        DataType::Year => 2,
        // v7.17.0 Phase 3.P0-34 — TIMETZ is i64 + i32 (12 bytes).
        DataType::TimeTz => 12,
        // v7.17.0 Phase 3.P0-35 — MONEY is fixed i64 (8 bytes).
        DataType::Money => 8,
        // v7.17.0 Phase 3.P0-38 — Range is variable-length (varlena).
        DataType::Range(_) => -1,
        // v7.17.0 Phase 3.P0-39 — Hstore is varlena.
        DataType::Hstore => -1,
        // v7.17.0 Phase 3.P0-40 — 2D arrays are varlena.
        DataType::IntArray2D | DataType::BigIntArray2D | DataType::TextArray2D => -1,
        _ => -1, // varlena
    }
}

/// v7.37.42-arena Phase 3 — fallback text renderer for the rare
/// cell types (Float / Numeric / Interval / Vector / Uuid / arrays /
/// hstore / range / etc.) the type-dispatched fast path in
/// `encode_pg_text_cell` doesn't handle directly. Pre-Phase 3 this
/// returned an owned `String` per cell, burning a heap alloc + drop
/// for every fallback-type cell on the wire. Now it allocates the
/// formatted text into the per-query `bumpalo::Bump` and yields a
/// `&'a str` whose lifetime is tied to the arena; the caller writes
/// the slice straight into the DataRow body and the arena bulk-drops
/// all per-cell payloads in O(1) at query-close. For inline-format
/// branches (Float / Numeric / Interval / Year / Vector / array
/// shapes) `bumpalo::format!` writes the formatted bytes directly
/// into the arena with no transient heap `String`. For
/// helper-returning-`String` branches (Uuid / Time / Money / Range /
/// Hstore / 2D arrays) we still allocate the helper's owned `String`,
/// then copy its bytes into the arena and drop the heap copy — these
/// types are sub-1% of fallback traffic so the helper churn doesn't
/// move the bench needle; the arena gives us a single uniform return
/// shape across all variants.
fn value_to_pg_text<'a>(
    v: &Value<'_>,
    ty: Option<DataType>,
    arena: &'a bumpalo::Bump,
    style: &spg_engine::eval::RenderStyle,
    tz: &spg_engine::SessionTz,
) -> Option<&'a str> {
    use bumpalo::collections::String as BumpString;
    use core::fmt::Write;

    let into_arena = |s: &str| -> &'a str { BumpString::from_str_in(s, arena).into_bump_str() };

    // Single-arg format helper — `bumpalo::format!` requires at least
    // one explicit `,arg` after the format string, so we use a manual
    // `BumpString::new_in + write!` for the single-capture-only sites
    // that just want `Display::fmt` into the arena.
    let display_into_arena = |d: &dyn core::fmt::Display| -> &'a str {
        let mut buf = BumpString::new_in(arena);
        let _ = write!(&mut buf, "{d}");
        buf.into_bump_str()
    };

    Some(match v {
        Value::Null => return None,
        Value::Bool(b) => {
            if *b {
                "t"
            } else {
                "f"
            }
        }
        Value::SmallInt(n) => display_into_arena(n),
        Value::Int(n) => display_into_arena(n),
        Value::BigInt(n) => display_into_arena(n),
        Value::Float(f) => into_arena(&spg_engine::eval::format_float_styled(*f, style)),
        // v7.39 — REAL renders PG float4out shortest-round-trip (it
        // previously fell through to the Debug placeholder).
        Value::Real(x) => into_arena(&spg_engine::eval::format_real_styled(*x, style)),
        Value::Text(s) | Value::Json(s) => into_arena(s.as_ref()),
        // v7.39 (bpchar epic) — bpchar's wire display is the PADDED
        // stored form (PG bpcharout).
        Value::BpChar(s) => into_arena(s.as_ref()),
        // v7.39 (FTS) — canonical tsvector/tsquery text (they fell
        // through to the Debug placeholder).
        Value::TsVector(lexs) => into_arena(&spg_engine::eval::format_tsvector(lexs)),
        Value::TsQuery(ast) => into_arena(&spg_engine::eval::format_tsquery(ast)),
        // v7.15.0 — TIMESTAMPTZ vs plain TIMESTAMP at render
        // time. mailrs round-8 acceptance: SELECT on TIMESTAMPTZ
        // must round-trip to a literal pg_dump would emit (i.e.
        // include the `+00` UTC offset).
        Value::Timestamp(micros) if matches!(ty, Some(DataType::Timestamptz)) => {
            // v7.39 (tz epic) — per-VALUE offset: a DST zone renders
            // -05 in January and -04 in July within one session.
            let off = tz.offset_at(*micros);
            let abbr = tz.abbrev_at(*micros);
            into_arena(&spg_engine::eval::format_timestamptz_tz(
                *micros,
                style,
                off,
                abbr.as_deref(),
            ))
        }
        Value::Timestamp(micros) => {
            into_arena(&spg_engine::eval::format_timestamp_styled(*micros, style))
        }
        Value::Date(days) => into_arena(&spg_engine::eval::format_date_styled(*days, style)),
        // v7.39 (GUC knife 2) — this arm emitted the internal
        // `P{m}M{d}D{u}U` codec form on the wire for years; PG text
        // format is what clients parse.
        Value::Interval {
            months,
            days,
            micros,
        } => into_arena(&spg_engine::eval::format_interval_styled(
            *months, *days, *micros, style,
        )),
        Value::Numeric {
            scaled,
            scale,
            kind,
        } => into_arena(&format_numeric_kind(*kind, *scaled, *scale)),
        Value::Vector(vec) => {
            // Inline join into the arena buffer — avoids
            // intermediate `Vec<String> + parts.join(", ")` heap
            // allocs (one per element + one for the join).
            let mut buf = BumpString::new_in(arena);
            buf.push('[');
            let mut first = true;
            for x in vec.iter() {
                if !first {
                    // pgvector's vector_out emits no spaces: [1,2,3].
                    buf.push_str(",");
                }
                first = false;
                use core::fmt::Write;
                let _ = write!(&mut buf, "{x}");
            }
            buf.push(']');
            buf.into_bump_str()
        }
        // v6.0.1: pgwire text-format render for SQ8 cells —
        // dequantise so clients see the pgvector-style
        // `[x, y, z]` payload.
        Value::Sq8Vector(q) => {
            let dequant = spg_storage::quantize::dequantize(q);
            let mut buf = BumpString::new_in(arena);
            buf.push('[');
            let mut first = true;
            for x in dequant.iter() {
                if !first {
                    buf.push_str(",");
                }
                first = false;
                use core::fmt::Write;
                let _ = write!(&mut buf, "{x}");
            }
            buf.push(']');
            buf.into_bump_str()
        }
        // v6.0.3: pgwire text-format render for HALF cells.
        Value::HalfVector(h) => {
            let halfs = h.to_f32_vec();
            let mut buf = BumpString::new_in(arena);
            buf.push('[');
            let mut first = true;
            for x in halfs.iter() {
                if !first {
                    buf.push_str(",");
                }
                first = false;
                use core::fmt::Write;
                let _ = write!(&mut buf, "{x}");
            }
            buf.push(']');
            buf.into_bump_str()
        }
        // v7.17.0 — UUID renders canonical 8-4-4-4-12 lowercase
        // hyphenated. Matches PG `uuid_out` so libpq clients,
        // psql `\d`, and sqlx text-mode decoders all read the
        // standard form.
        Value::Uuid(b) => into_arena(&spg_storage::format_uuid(b)),
        // v7.17.0 Phase 3.P0-32 — TIME renders via the shared
        // engine helper so the trim-trailing-zeros shape matches
        // PG `time_out` across pgwire, sqllogictest, and engine.
        Value::Time(us) => into_arena(&spg_engine::eval::format_time(*us)),
        // v7.17.0 Phase 3.P0-33 — YEAR renders 4-digit zero-padded.
        Value::Year(y) => {
            let mut buf = BumpString::new_in(arena);
            let _ = write!(&mut buf, "{y:04}");
            buf.into_bump_str()
        }
        // v7.17.0 Phase 3.P0-34 — TIMETZ via the shared engine
        // helper so the canonical `HH:MM:SS[.ffffff]±HH[:MM]`
        // shape matches PG `timetz_out` across all renderers.
        Value::TimeTz { us, offset_secs } => {
            into_arena(&spg_engine::eval::format_timetz(*us, *offset_secs))
        }
        // v7.17.0 Phase 3.P0-35 — MONEY via the shared engine
        // helper so the canonical en_US text form matches PG
        // `cash_out` across all renderers.
        Value::Money(c) => into_arena(&spg_engine::eval::format_money(*c)),
        // v7.17.0 Phase 3.P0-38 — Range via shared engine helper.
        Value::Range { .. } => into_arena(&spg_engine::format_range_text(v)),
        // v7.17.0 Phase 3.P0-39 — Hstore via shared engine helper.
        Value::Hstore(pairs) => into_arena(&spg_engine::format_hstore_text(pairs)),
        // v7.17.0 Phase 3.P0-40 — 2D arrays via shared helpers.
        Value::IntArray2D(rows) => into_arena(&spg_engine::format_int_2d_text_pub(rows)),
        Value::BigIntArray2D(rows) => into_arena(&spg_engine::format_bigint_2d_text_pub(rows)),
        Value::TextArray2D(rows) => into_arena(&spg_engine::format_text_2d_text_pub(rows)),
        // v7.39 — the 1-D array family was MISSING here entirely and
        // fell through to the Debug placeholder (found when
        // array_agg over the wire printed `IntArray([Some(5), …])`).
        // Canonical PG array text via the shared eval formatters.
        Value::TextArray(items) => into_arena(&spg_engine::eval::format_text_array(items)),
        Value::IntArray(items) => into_arena(&spg_engine::eval::format_int_array(items)),
        Value::BigIntArray(items) => into_arena(&spg_engine::eval::format_bigint_array(items)),
        Value::BoolArray(items) => into_arena(&spg_engine::eval::format_bool_array(items)),
        Value::SmallIntArray(items) => into_arena(&spg_engine::eval::format_smallint_array(items)),
        Value::FloatArray(items) => {
            into_arena(&spg_engine::eval::format_float_array_styled(items, style))
        }
        Value::NumericArray(items) => into_arena(&spg_engine::eval::format_numeric_array(items)),
        Value::DateArray(items) => {
            into_arena(&spg_engine::eval::format_date_array_styled(items, style))
        }
        Value::TimestampArray(items) => into_arena(
            &spg_engine::eval::format_timestamp_array_styled(items, false, style),
        ),
        Value::TimestamptzArray(items) => {
            // Element-wise per-value offsets (see the scalar arm).
            let mut out = String::from("{");
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                match item {
                    None => out.push_str("NULL"),
                    Some(t) => {
                        let abbr = tz.abbrev_at(*t);
                        out.push_str(&spg_engine::eval::format_timestamptz_tz(
                            *t,
                            style,
                            tz.offset_at(*t),
                            abbr.as_deref(),
                        ));
                    }
                }
            }
            out.push('}');
            into_arena(&out)
        }
        Value::UuidArray(items) => into_arena(&spg_engine::eval::format_uuid_array(items)),
        Value::IntervalArray(items) => into_arena(&spg_engine::eval::format_interval_array_styled(
            items, style,
        )),
        // v7.39 — every remaining variant renders its canonical PG
        // text via the engine's shared formatter (the old Debug
        // fallback leaked `Inet { .. }` / `Point(..)` / `BitString
        // { .. }` placeholders for sixteen types on the wire).
        other => into_arena(&spg_engine::eval::value_to_text_styled(other, style)),
    })
}

/// Render a `Timestamp(micros)` as ISO-8601 microsecond precision in
/// UTC. We don't have chrono in this crate, so the formatting is
/// done from scratch — good enough for psql `\d` output.
fn format_timestamp(micros: i64) -> String {
    let secs = micros.div_euclid(1_000_000);
    let frac = micros.rem_euclid(1_000_000) as u32;
    let (y, m, d, hh, mm, ss) = secs_to_ymdhms(secs);
    if frac == 0 {
        format!("{y:04}-{m:02}-{d:02} {hh:02}:{mm:02}:{ss:02}")
    } else {
        format!("{y:04}-{m:02}-{d:02} {hh:02}:{mm:02}:{ss:02}.{frac:06}")
    }
}

fn format_date(days: i32) -> String {
    let secs = i64::from(days) * 86_400;
    let (y, m, d, _, _, _) = secs_to_ymdhms(secs);
    format!("{y:04}-{m:02}-{d:02}")
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn secs_to_ymdhms(secs: i64) -> (i32, u32, u32, u32, u32, u32) {
    // Howard Hinnant's days-from-epoch / civil-from-days algorithm
    // adapted from the public-domain reference at
    // https://howardhinnant.github.io/date_algorithms.html.
    let day = secs.div_euclid(86_400);
    let tod = secs.rem_euclid(86_400) as u32;
    let hh = tod / 3600;
    let mm = (tod / 60) % 60;
    let ss = tod % 60;
    // civil_from_days expects "days since 1970-01-01"; the algorithm
    // is named for "shifted" days from 0000-03-01.
    let z = day + 719_468;
    let era = z.div_euclid(146_097);
    let doe = (z - era * 146_097) as u32;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y_int = yoe as i32 + (era as i32) * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y_int + 1 } else { y_int };
    (y, m, d, hh, mm, ss)
}

fn format_numeric_kind(kind: spg_storage::NumericKind, scaled: i128, scale: u16) -> String {
    use spg_storage::NumericKind;
    match kind {
        NumericKind::Finite => format_numeric(scaled, scale),
        NumericKind::NaN => "NaN".to_string(),
        NumericKind::PosInf => "Infinity".to_string(),
        NumericKind::NegInf => "-Infinity".to_string(),
    }
}

fn format_numeric(scaled: i128, scale: u16) -> String {
    if scale == 0 {
        return scaled.to_string();
    }
    let s = scaled.abs().to_string();
    let scale = scale as usize;
    let (int_part, frac_part) = if s.len() > scale {
        let split = s.len() - scale;
        (&s[..split], &s[split..])
    } else {
        ("0", s.as_str())
    };
    let mut frac_pad = "0".repeat(scale.saturating_sub(frac_part.len()));
    frac_pad.push_str(frac_part);
    if scaled < 0 {
        format!("-{int_part}.{frac_pad}")
    } else {
        format!("{int_part}.{frac_pad}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_tag_derives_verb_from_ast_not_first_word() {
        // v7.38 (read01 P3.27) — plain statements tag off the first word.
        assert_eq!(command_tag("INSERT INTO t VALUES (1)", 1), "INSERT 0 1");
        assert_eq!(command_tag("UPDATE t SET x = 1", 4), "UPDATE 4");
        assert_eq!(command_tag("DELETE FROM t", 2), "DELETE 2");
        // A data-modifying CTE is named by its real top-level verb, not WITH.
        assert_eq!(
            command_tag("WITH c AS (SELECT 1) INSERT INTO t SELECT * FROM c", 3),
            "INSERT 0 3"
        );
        assert_eq!(
            command_tag("WITH c AS (SELECT 1) UPDATE t SET x = 1", 5),
            "UPDATE 5"
        );
        assert_eq!(
            command_tag("WITH c AS (SELECT 1) DELETE FROM t WHERE id > 9", 0),
            "DELETE 0"
        );
        // v7.39 (read01 utils/adt, enum.c) — the TYPE family is a
        // two-word tag; other DDL keeps the first-word fallback.
        assert_eq!(
            command_tag("CREATE TYPE mood AS ENUM ('a')", 0),
            "CREATE TYPE"
        );
        assert_eq!(
            command_tag("ALTER TYPE mood ADD VALUE 'b'", 0),
            "ALTER TYPE"
        );
        assert_eq!(command_tag("DROP TYPE IF EXISTS mood", 0), "DROP TYPE");
        assert_eq!(command_tag("CREATE TABLE t (id INT)", 0), "CREATE TABLE");
        assert_eq!(
            command_tag("CREATE UNIQUE INDEX i ON t (id)", 0),
            "CREATE INDEX"
        );
        assert_eq!(
            command_tag("CREATE OR REPLACE VIEW v AS SELECT 1", 0),
            "CREATE VIEW"
        );
        assert_eq!(
            command_tag("CREATE TEMP TABLE t (id INT)", 0),
            "CREATE TABLE"
        );
        assert_eq!(command_tag("DROP TABLE IF EXISTS t", 0), "DROP TABLE");
        assert_eq!(
            command_tag("ALTER TABLE t ADD COLUMN x INT", 0),
            "ALTER TABLE"
        );
        assert_eq!(command_tag("TRUNCATE t", 0), "TRUNCATE TABLE");
        assert_eq!(command_tag("CREATE USER u", 0), "CREATE ROLE");
        assert_eq!(
            command_tag("DROP MATERIALIZED VIEW m", 0),
            "DROP MATERIALIZED VIEW"
        );
        // CREATE MATERIALIZED VIEW is a recorded delta (PG: SELECT <n>).
        assert_eq!(
            command_tag("CREATE MATERIALIZED VIEW m AS SELECT 1", 0),
            "CREATE"
        );
        assert_eq!(
            command_tag("CREATE EXTENSION pgcrypto", 0),
            "CREATE EXTENSION"
        );
        assert_eq!(
            command_tag("REFRESH MATERIALIZED VIEW m", 0),
            "REFRESH MATERIALIZED VIEW"
        );
    }

    fn read_cell(buf: &[u8]) -> &[u8] {
        let len = i32::from_be_bytes(buf[..4].try_into().unwrap());
        assert!(len >= 0, "negative cell length");
        let len = len as usize;
        &buf[4..4 + len]
    }

    // v7.37.43-T4 — `split_top_level_statements` is the splitter that
    // turns sqlx::migrate!()'s multi-statement Q-message into the
    // per-statement dispatch loop. The acceptance cases here lock
    // every PG lexer boundary the splitter must respect — the
    // sentori migrations exercise all of them.
    fn split_owned(body: &str) -> Vec<String> {
        split_top_level_statements(body.as_bytes())
            .into_iter()
            .map(|s| String::from_utf8(s.to_vec()).unwrap())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect()
    }

    #[test]
    fn splitter_single_statement_no_semicolon() {
        assert_eq!(split_owned("SELECT 1"), vec!["SELECT 1".to_string()]);
    }

    #[test]
    fn splitter_two_statements_semicolon_separated() {
        let r = split_owned("CREATE TABLE a (); CREATE TABLE b ();");
        assert_eq!(r, vec!["CREATE TABLE a ()", "CREATE TABLE b ()"]);
    }

    #[test]
    fn splitter_ignores_semicolon_inside_single_quotes() {
        // CHECK (kind IN ('public', 'admin')) — sentori 0001_init.sql
        // ships exactly this shape; a naive splitter would split on
        // the `;` inside the literal of an earlier statement.
        let r = split_owned("INSERT INTO t VALUES ('a;b'); SELECT 1");
        assert_eq!(r, vec!["INSERT INTO t VALUES ('a;b')", "SELECT 1"]);
    }

    #[test]
    fn splitter_ignores_semicolon_inside_double_quoted_ident() {
        let r = split_owned(r#"SELECT "a;b"; SELECT 1"#);
        assert_eq!(r, vec![r#"SELECT "a;b""#, "SELECT 1"]);
    }

    #[test]
    fn splitter_ignores_semicolon_inside_line_comment() {
        let r = split_owned("SELECT 1 -- comment ; nope\n; SELECT 2");
        assert_eq!(r, vec!["SELECT 1 -- comment ; nope", "SELECT 2"]);
    }

    #[test]
    fn splitter_ignores_semicolon_inside_block_comment() {
        let r = split_owned("SELECT 1 /* ; not a split */; SELECT 2");
        assert_eq!(r, vec!["SELECT 1 /* ; not a split */", "SELECT 2"]);
    }

    #[test]
    fn splitter_handles_dollar_quoted_do_block() {
        // sentori `0009_quotas.sql` shape — a DO $$ … $$ block carrying
        // an inner statement that ends in `;`. The splitter must NOT
        // break the script at the inner `;` because it's inside the
        // dollar-quoted body; only the closing `$$;` ends the DO.
        let script = "DO $$ BEGIN \
                     IF NOT EXISTS (SELECT 1) THEN \
                       CREATE TYPE foo AS ENUM ('a'); \
                     END IF; \
                     END $$; \
                     CREATE TABLE t ()";
        let r = split_owned(script);
        assert_eq!(r.len(), 2, "want 2 stmts, got: {r:?}");
        assert!(r[0].starts_with("DO $$"));
        assert_eq!(r[1], "CREATE TABLE t ()");
    }

    #[test]
    fn splitter_handles_tagged_dollar_quotes() {
        let script = "SELECT $tag$body; with ; semicolons$tag$; SELECT 1";
        let r = split_owned(script);
        assert_eq!(r.len(), 2);
        assert_eq!(r[1], "SELECT 1");
    }

    #[test]
    fn splitter_drops_empty_statements_between_semicolons() {
        let r = split_owned("SELECT 1;;; SELECT 2;");
        assert_eq!(r, vec!["SELECT 1", "SELECT 2"]);
    }

    #[test]
    fn splitter_handles_escaped_single_quote_in_string() {
        // PG escapes `'` inside a string by doubling: `'it''s ok'`.
        // The splitter must not break out of the literal early.
        let r = split_owned("SELECT 'it''s; ok'; SELECT 1");
        assert_eq!(r, vec!["SELECT 'it''s; ok'", "SELECT 1"]);
    }

    /// v7.34.6 — `write_cell_timestamp` must match
    /// `spg_engine::eval::format_timestamp` byte-for-byte on the
    /// in-range path (year 0..=9999), since the PROJ DataRow output
    /// flows back into clients that already see the engine
    /// formatter's output via other paths.
    #[test]
    fn write_cell_timestamp_matches_engine_format() {
        let cases: &[i64] = &[
            0,                       // 1970-01-01 00:00:00
            1_700_000_000_000_000,   // 2023-11-14 22:13:20
            1_700_000_000_123_456,   // …+frac
            1_700_000_000_123_000,   // …+frac trailing zero trim
            1_700_000_000_100_000,   // …+frac more trailing zero trim
            -1_000_000_000_000,      // 1969-12-20 10:13:20 (pre-epoch)
            253_402_300_799_000_000, // 9999-12-31 23:59:59
            1_577_836_800_000_000,   // 2020-01-01 00:00:00
        ];
        for &micros in cases {
            let expected = spg_engine::eval::format_timestamp(micros);
            let mut buf = Vec::new();
            write_cell_timestamp(&mut buf, micros, false).unwrap();
            let got = std::str::from_utf8(read_cell(&buf)).unwrap();
            assert_eq!(got, expected, "timestamp mismatch @ micros={micros}");

            let expected_tz = spg_engine::eval::format_timestamptz(micros);
            let mut buf_tz = Vec::new();
            write_cell_timestamp(&mut buf_tz, micros, true).unwrap();
            let got_tz = std::str::from_utf8(read_cell(&buf_tz)).unwrap();
            assert_eq!(
                got_tz, expected_tz,
                "timestamptz mismatch @ micros={micros}"
            );
        }
    }

    #[test]
    fn write_cell_date_matches_pgwire_format() {
        let cases: &[i32] = &[
            0,         // 1970-01-01
            19_723,    // 2024-01-01
            -10_957,   // 1940-01-02
            2_932_896, // 9999-12-31
        ];
        for &days in cases {
            let expected = format_date(days);
            let mut buf = Vec::new();
            write_cell_date(&mut buf, days).unwrap();
            let got = std::str::from_utf8(read_cell(&buf)).unwrap();
            assert_eq!(got, expected, "date mismatch @ days={days}");
        }
    }

    /// v7.35.0 — `write_cell_bytes` byte-level equivalence vs the
    /// pre-7.35 `extend_from_slice` form. The new path uses a
    /// single `reserve` + two `copy_nonoverlapping` writes inside
    /// an `unsafe` block; this test pins the output byte-for-byte
    /// across empty / single-byte / mid-size / large payloads, so
    /// any regression on the `unsafe` accounting fails loudly.
    #[test]
    fn write_cell_bytes_matches_extend_form() {
        fn extend_form(out: &mut Vec<u8>, bytes: &[u8]) {
            let len = i32::try_from(bytes.len()).unwrap();
            out.extend_from_slice(&len.to_be_bytes());
            out.extend_from_slice(bytes);
        }
        let cases: &[&[u8]] = &[
            b"",
            b"a",
            b"hello",
            b"abcdefghij" as &[u8],
            &[0u8; 256],
            &[0xffu8; 1024],
        ];
        for bytes in cases {
            let mut got = Vec::new();
            write_cell_bytes(&mut got, bytes).unwrap();
            let mut want = Vec::new();
            extend_form(&mut want, bytes);
            assert_eq!(got, want, "len={}", bytes.len());
        }
    }

    /// v7.15.0 — pg_dump's default output emits `COPY t (col, col)
    /// FROM stdin;` for every table with data. The old whitespace-
    /// split parser mistook `(col,` for the FROM direction word and
    /// missed the COPY entirely; the new bytes-walk skips the
    /// parenthesised column list.
    #[test]
    fn parse_copy_with_column_list() {
        let sql = "COPY posts (id, title, body) FROM stdin;";
        match parse_copy_intent(sql) {
            Some(CopyIntent::From(table, _, _)) => assert_eq!(table, "posts"),
            other => panic!("expected From(posts), got {other:?}"),
        }
    }

    #[test]
    fn parse_copy_without_column_list() {
        let sql = "COPY accounts FROM STDIN";
        match parse_copy_intent(sql) {
            Some(CopyIntent::From(table, _, _)) => assert_eq!(table, "accounts"),
            other => panic!("expected From(accounts), got {other:?}"),
        }
    }

    #[test]
    fn parse_copy_to_stdout_with_column_list() {
        let sql = "COPY t (a, b) TO STDOUT";
        match parse_copy_intent(sql) {
            Some(CopyIntent::To(table, _)) => assert_eq!(table, "t"),
            other => panic!("expected To(t), got {other:?}"),
        }
    }

    #[test]
    fn parse_copy_query_to_stdout() {
        // v7.39 (read01 round 94) — the parenthesised query form.
        let sql =
            "COPY (SELECT a, b FROM t WHERE a > 1 ORDER BY a) TO STDOUT WITH (FORMAT csv, HEADER)";
        match parse_copy_intent(sql) {
            Some(CopyIntent::ToQuery(query, opts)) => {
                assert_eq!(query, "SELECT a, b FROM t WHERE a > 1 ORDER BY a");
                assert!(opts.format_csv);
                assert!(opts.header);
            }
            other => panic!("expected ToQuery, got {other:?}"),
        }
    }

    #[test]
    fn parse_copy_query_with_cte_not_confused_by_inner_with() {
        // The inner `WITH` CTE must not be mistaken for the COPY option list.
        let sql = "COPY (WITH x AS (SELECT 1 AS n) SELECT n FROM x) TO STDOUT";
        match parse_copy_intent(sql) {
            Some(CopyIntent::ToQuery(query, opts)) => {
                assert_eq!(query, "WITH x AS (SELECT 1 AS n) SELECT n FROM x");
                assert!(!opts.format_csv);
                assert!(!opts.header);
            }
            other => panic!("expected ToQuery, got {other:?}"),
        }
    }

    #[test]
    fn parse_copy_with_with_options() {
        let sql = "COPY t FROM stdin WITH (format json)";
        match parse_copy_intent(sql) {
            Some(CopyIntent::From(table, _, opts)) => {
                assert_eq!(table, "t");
                assert!(opts.format_json);
            }
            other => panic!("expected From(t) with format_json, got {other:?}"),
        }
    }

    #[test]
    fn parse_to_file_intent() {
        match parse_copy_intent("COPY ct TO '/tmp/spg-r252/x.txt'") {
            Some(CopyIntent::ToFile(spec)) => {
                assert_eq!(spec.table, "ct");
                assert_eq!(spec.path, "/tmp/spg-r252/x.txt");
            }
            other => panic!("expected ToFile, got {other:?}"),
        }
    }

    #[test]
    fn parse_non_copy_returns_none() {
        assert!(parse_copy_intent("SELECT 1").is_none());
        // v7.39 (round 251) — file-based COPY is now a real intent: the
        // SERVER reads the file (PG semantics; admin-gated in the
        // handler, 42501 for everyone else).
        match parse_copy_intent("COPY t FROM '/etc/passwd'") {
            Some(CopyIntent::FromFile(spec)) => {
                assert_eq!(spec.table, "t");
                assert_eq!(spec.path, "/etc/passwd");
            }
            other => panic!("expected FromFile, got {other:?}"),
        }
    }

    #[test]
    fn parse_copy_from_csv_options() {
        let sql = "COPY t FROM stdin WITH (FORMAT csv, HEADER true, DELIMITER ';', QUOTE '#')";
        match parse_copy_intent(sql) {
            Some(CopyIntent::From(table, _, opts)) => {
                assert_eq!(table, "t");
                assert!(opts.format_csv);
                assert!(!opts.format_json);
                assert_eq!(opts.skip, 1); // HEADER → skip the header row
                assert_eq!(opts.csv_delimiter, Some(';'));
                assert_eq!(opts.csv_quote, Some('#'));
            }
            other => panic!("expected From(t) csv opts, got {other:?}"),
        }
    }

    #[test]
    fn parse_copy_from_bare_csv_header() {
        // Bare HEADER (no boolean) still skips the header row.
        let sql = "COPY t FROM stdin WITH (FORMAT csv, HEADER)";
        match parse_copy_intent(sql) {
            Some(CopyIntent::From(_, _, opts)) => {
                assert!(opts.format_csv);
                assert_eq!(opts.skip, 1);
            }
            other => panic!("expected csv, got {other:?}"),
        }
    }

    /// v7.37.5 α — `decode_binary_param` OID 2950 (UUID) round-trips the
    /// raw 16-byte RFC 4122 payload that sqlx-postgres and other typed
    /// drivers send for typed `Uuid` parameters in binary format.
    #[test]
    fn decode_binary_param_uuid_16_bytes_round_trip() {
        let bytes = [
            0x55, 0x0e, 0x84, 0x00, 0xe2, 0x9b, 0x41, 0xd4, 0xa7, 0x16, 0x44, 0x66, 0x55, 0x44,
            0x00, 0x00,
        ];
        let v = decode_binary_param(2950, &bytes).expect("UUID binary BIND must succeed");
        match v {
            spg_storage::Value::Uuid(b) => assert_eq!(b, bytes),
            other => panic!("expected Value::Uuid, got {other:?}"),
        }
    }

    #[test]
    fn decode_binary_param_uuid_rejects_wrong_length() {
        for &len in &[0usize, 1, 8, 15, 17, 32] {
            let bytes = vec![0u8; len];
            let r = decode_binary_param(2950, &bytes);
            assert!(r.is_err(), "len={len} must reject (UUID is 16 bytes)");
        }
    }

    /// v7.37.5 β-P3 — `decode_binary_param` OID 1186 (INTERVAL)
    /// round-trips the PG-canonical 16-byte payload:
    /// `i64 BE micros + i32 BE days + i32 BE months`. The
    /// engine-side `Value::Interval` field order is host-independent
    /// (named fields), so this verifies the byte-swap into the
    /// internal three-field shape rather than the SPG codec's LE
    /// disk format.
    #[test]
    fn decode_binary_param_interval_16_bytes_round_trip() {
        // INTERVAL '1 month 2 days 3 microseconds':
        //   months = 1, days = 2, micros = 3.
        let mut bytes = [0u8; 16];
        bytes[0..8].copy_from_slice(&3_i64.to_be_bytes()); // micros
        bytes[8..12].copy_from_slice(&2_i32.to_be_bytes()); // days
        bytes[12..16].copy_from_slice(&1_i32.to_be_bytes()); // months
        let v = decode_binary_param(1186, &bytes).expect("INTERVAL binary BIND must succeed");
        assert_eq!(
            v,
            spg_storage::Value::Interval {
                months: 1,
                days: 2,
                micros: 3,
            }
        );
    }

    /// Negative dimensions thread through unchanged (PG INTERVAL
    /// allows them — `INTERVAL '-1 day' = INTERVAL '-1 day'`).
    #[test]
    fn decode_binary_param_interval_signed_round_trip() {
        let mut bytes = [0u8; 16];
        bytes[0..8].copy_from_slice(&(-86_400_000_000_i64).to_be_bytes());
        bytes[8..12].copy_from_slice(&(-1_i32).to_be_bytes());
        bytes[12..16].copy_from_slice(&(-1_i32).to_be_bytes());
        let v =
            decode_binary_param(1186, &bytes).expect("signed INTERVAL binary BIND must succeed");
        assert_eq!(
            v,
            spg_storage::Value::Interval {
                months: -1,
                days: -1,
                micros: -86_400_000_000,
            }
        );
    }

    #[test]
    fn decode_binary_param_interval_rejects_wrong_length() {
        for &len in &[0usize, 1, 8, 12, 15, 17, 32] {
            let bytes = vec![0u8; len];
            let r = decode_binary_param(1186, &bytes);
            assert!(
                r.is_err(),
                "len={len} must reject (INTERVAL binary is 16 bytes)"
            );
        }
    }
}
