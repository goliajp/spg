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

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::thread;

use spg_engine::{CancelToken, EngineError, MonotonicNowFn, QueryResult, Role};
use spg_storage::{ColumnSchema, DataType, Row, Value};

use crate::ServerState;

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

fn handle_conn(mut stream: TcpStream, state: &Arc<ServerState>) -> std::io::Result<()> {
    let _ = stream.set_nodelay(true);

    // ---- Startup phase ----
    let (user, params) = read_startup(&mut stream)?;
    // v7.17 Phase 2.4 — surface `application_name` from the startup
    // params on the per-connection ConnState (read back via
    // `spg_stat_activity.application_name`). Other params
    // (database / options / etc.) we still ignore.
    let startup_app_name = params
        .iter()
        .find_map(|(k, v)| (k == "application_name").then(|| v.clone()))
        .unwrap_or_default();

    // v6.5.2 — register this connection in the activity registry.
    // Removed when `_conn_guard` drops at function exit.
    let conn_state = Arc::new(crate::ConnState {
        pid: std::process::id().wrapping_add(
            state
                .active_connections
                .load(std::sync::atomic::Ordering::Relaxed) as u32,
        ),
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
    });
    if let Ok(mut conns) = state.connections.write() {
        conns.push(Arc::clone(&conn_state));
    }
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
            scram_auth(&mut stream, state, &user)?
        } else {
            cleartext_auth(&mut stream, state, &user)?
        };
        match outcome {
            Some(r) => r,
            None => return Ok(()), // error already sent
        }
    } else {
        Role::Admin
    };

    // AuthenticationOk
    send_msg(&mut stream, b'R', &0u32.to_be_bytes())?;
    // ParameterStatus pairs — keep the set minimal but include the
    // ones psql / driver libraries check first.
    send_parameter_status(&mut stream, "server_version", "16.0 (spg-4.3)")?;
    send_parameter_status(&mut stream, "client_encoding", "UTF8")?;
    send_parameter_status(&mut stream, "DateStyle", "ISO, MDY")?;
    send_parameter_status(&mut stream, "integer_datetimes", "on")?;
    send_parameter_status(&mut stream, "standard_conforming_strings", "on")?;
    // BackendKeyData — required by spec but we don't support cancel,
    // so the keys are bogus. Most clients ignore.
    let mut bkd = Vec::with_capacity(8);
    bkd.extend_from_slice(&std::process::id().to_be_bytes());
    bkd.extend_from_slice(&0u32.to_be_bytes());
    send_msg(&mut stream, b'K', &bkd)?;
    send_ready_for_query(&mut stream, b'I')?;

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
    loop {
        let mut header = [0u8; 5];
        if let Err(e) = stream.read_exact(&mut header) {
            if e.kind() == std::io::ErrorKind::UnexpectedEof {
                return Ok(());
            }
            return Err(e);
        }
        let msg_type = header[0];
        let len = u32::from_be_bytes([header[1], header[2], header[3], header[4]]) as usize;
        // PG length includes the 4 bytes of the length itself.
        let body_len = len.saturating_sub(4);
        let mut body = vec![0u8; body_len];
        if body_len > 0 {
            stream.read_exact(&mut body)?;
        }

        match msg_type {
            b'Q' => {
                // Null-terminated SQL string (typically — psql appends \0).
                let sql_bytes = body.strip_suffix(b"\0").unwrap_or(&body);
                // v6.5.2 — update activity registry.
                let now_us = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_micros() as i64)
                    .unwrap_or(0);
                conn_state
                    .last_query_start_us
                    .store(now_us, std::sync::atomic::Ordering::Relaxed);
                if let Ok(mut s) = conn_state.current_sql.write() {
                    *s = String::from_utf8_lossy(sql_bytes).to_string();
                }
                let Ok(sql_str) = std::str::from_utf8(sql_bytes) else {
                    send_error(&mut wbuf, "22021", "invalid UTF-8 in query")?;
                    send_ready_for_query(&mut wbuf, tx_state)?;
                    stream.write_all(&wbuf)?;
                    wbuf.clear();
                    continue;
                };
                let sql = sql_str.trim_end_matches(';').trim().to_string();
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
                if let Some((name, value)) = parse_set_statement(&sql) {
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
                    let engine_affecting = matches!(
                        name_lc.as_str(),
                        "foreign_key_checks"
                            | "session_replication_role"
                            | "default_text_search_config"
                    );
                    // Detect multi-assignment from the LHS shape
                    // (contains '@' anywhere) or comma in the value
                    // portion (more than one assignment).
                    let is_multi = name_lc.contains('@')
                        || value.contains(',')
                        || sql.to_ascii_lowercase().contains("foreign_key_checks=0")
                        || sql.to_ascii_lowercase().contains("foreign_key_checks =")
                        || sql.to_ascii_lowercase().contains("foreign_key_checks  ");
                    if !engine_affecting && !is_multi {
                        send_command_complete(&mut wbuf, "SET")?;
                        send_ready_for_query(&mut wbuf, tx_state)?;
                        stream.write_all(&wbuf)?;
                        wbuf.clear();
                        continue;
                    }
                    // engine-affecting or multi-assignment:
                    // fall through to dispatch so the engine sees it.
                }
                // v4.19: SHOW name / SHOW ALL.
                if let Some(name) = parse_show_statement(&sql) {
                    let resp = render_show(&name, &settings);
                    send_canned(&mut wbuf, &resp)?;
                    send_ready_for_query(&mut wbuf, tx_state)?;
                    stream.write_all(&wbuf)?;
                    wbuf.clear();
                    continue;
                }
                if let Some(copy) = parse_copy_intent(&sql) {
                    // COPY mode handles its own protocol roundtrips —
                    // flush any pending output so the COPY handler
                    // starts from a clean wire state.
                    if !wbuf.is_empty() {
                        stream.write_all(&wbuf)?;
                        wbuf.clear();
                    }
                    match copy {
                        CopyIntent::From(table, opts) => {
                            handle_copy_from_stdin(
                                &mut stream,
                                state,
                                role,
                                &table,
                                &opts,
                                &mut tx_state,
                            )?;
                        }
                        CopyIntent::To(table) => {
                            handle_copy_to_stdout(&mut stream, state, role, &table, &mut tx_state)?;
                        }
                    }
                    send_ready_for_query(&mut wbuf, tx_state)?;
                    stream.write_all(&wbuf)?;
                    wbuf.clear();
                    continue;
                }
                // psql sends startup probes like "SELECT version()" /
                // "SHOW search_path". Stub the common ones with sane
                // canned answers so the client doesn't error out.
                if let Some(canned) = canned_response(&sql, state) {
                    send_canned(&mut wbuf, &canned)?;
                    send_ready_for_query(&mut wbuf, tx_state)?;
                    stream.write_all(&wbuf)?;
                    wbuf.clear();
                    continue;
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
                let cancel = statement_cancel(&settings);
                let result = execute_with_role(state, &sql, role, cancel);
                conn_state
                    .wait_event
                    .store(0, std::sync::atomic::Ordering::Relaxed);
                match result {
                    Ok(QueryResult::Rows { columns, rows }) => {
                        send_row_description(&mut wbuf, &columns)?;
                        let n = rows.len();
                        for row in &rows {
                            send_data_row(&mut wbuf, &columns, row)?;
                        }
                        send_command_complete(&mut wbuf, &format!("SELECT {n}"))?;
                    }
                    Ok(QueryResult::CommandOk {
                        affected,
                        modified_catalog,
                    }) => {
                        let tag = command_tag(&sql, affected);
                        send_command_complete(&mut wbuf, &tag)?;
                        // v6.5.3 — audit-log every catalog-mutating
                        // statement from the pgwire path (matches the
                        // native-wire path's audit hook). Silent on
                        // mutex poisoning / write error to keep the
                        // hot path simple; broken audit chains surface
                        // via spg_audit_verify.
                        if modified_catalog && state.audit_path.is_some() {
                            let _ = crate::append_audit_pub(state, &sql);
                        }
                        // Sync tx state from engine after writes.
                        tx_state = if state.engine.read().is_ok_and(|e| e.in_transaction()) {
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
                        let (sqlstate, msg) = engine_error_to_wire(&e);
                        send_error(&mut wbuf, sqlstate, &msg)?;
                        // After an error inside a TX, PG goes to 'E'
                        // and stays there until ROLLBACK. We track
                        // best-effort: if engine still in TX, mark
                        // 'E'; otherwise 'I'.
                        tx_state = if state.engine.read().is_ok_and(|e| e.in_transaction()) {
                            b'E'
                        } else {
                            b'I'
                        };
                    }
                    // v7.5.0 — QueryResult is #[non_exhaustive].
                    Ok(_) => {
                        send_error(&mut wbuf, "XX000", "unexpected QueryResult variant")?;
                    }
                }
                send_ready_for_query(&mut wbuf, tx_state)?;
                stream.write_all(&wbuf)?;
                wbuf.clear();
            }
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
                if let Err(msg) = handle_parse(&body, &mut prepared, state) {
                    send_error(&mut wbuf, "42601", &msg)?;
                } else {
                    send_msg(&mut wbuf, b'1', &[])?;
                }
            }
            // Bind (B): create a portal with parameter values
            // substituted into the prepared statement's SQL.
            b'B' => {
                match handle_bind(&body, &prepared) {
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
                    let name = cstring_at(&body, 1).unwrap_or_default();
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
                    &body,
                    &portals,
                    &prepared,
                    &settings,
                    &mut wbuf,
                    state,
                    role,
                    &mut tx_state,
                ) {
                    send_error(&mut wbuf, sqlstate, &msg)?;
                }
            }
            // Close (C): drop the named statement or portal. Reply
            // CloseComplete.
            b'C' => {
                if body.len() >= 2 {
                    let kind = body[0];
                    let name = cstring_at(&body, 1).unwrap_or_default();
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

fn execute_with_role(
    state: &Arc<ServerState>,
    sql: &str,
    role: Role,
    cancel: CancelToken<'_>,
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
    if is_read {
        let lower_sql = sql.to_ascii_lowercase();
        if lower_sql.contains("setval(")
            || lower_sql.contains("nextval(")
            || lower_sql.contains("currval(")
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
    if is_read {
        let engine = state
            .engine
            .read()
            .map_err(|_| EngineError::Unsupported("engine rwlock poisoned".into()))?;
        engine.execute_readonly_with_cancel(sql, cancel)
    } else {
        let mut engine = state
            .engine
            .write()
            .map_err(|_| EngineError::Unsupported("engine rwlock poisoned".into()))?;
        engine.execute_with_cancel(sql, cancel)
    }
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
        "COMMIT" => "COMMIT".to_string(),
        "ROLLBACK" => "ROLLBACK".to_string(),
        other => other.to_string(), // CREATE TABLE / DROP USER / etc.
    }
}

/// Canned answers for client startup probes + the v4.6 pg_catalog
/// subset. Saves us implementing real pg_class / pg_namespace / etc.
/// tables in the engine just to make `psql` and friends not bail on
/// connect. The patterns matched here are exact-prefix lowercased
/// matches; anything stranger drops through to the engine, which
/// will reject pg_catalog table names with a clear "not found"
/// error.
fn canned_response(sql: &str, state: &Arc<ServerState>) -> Option<CannedResponse> {
    let lower = sql.trim().to_ascii_lowercase();
    if lower.starts_with("select version()") || lower == "select version()" {
        return Some(CannedResponse::single_text("version", "spg 4.6"));
    }
    if lower.starts_with("show transaction_isolation")
        || lower.starts_with("show transaction isolation level")
    {
        return Some(CannedResponse::single_text(
            "transaction_isolation",
            "read committed",
        ));
    }
    if lower.starts_with("show search_path") || lower == "show search_path" {
        return Some(CannedResponse::single_text(
            "search_path",
            "\"$user\", public",
        ));
    }
    if lower.starts_with("show standard_conforming_strings") {
        return Some(CannedResponse::single_text(
            "standard_conforming_strings",
            "on",
        ));
    }
    if lower.starts_with("select current_database()") || lower == "select current_database()" {
        return Some(CannedResponse::single_text("current_database", "spg"));
    }
    if lower.starts_with("select current_schema()")
        || lower == "select current_schema()"
        || lower == "select current_schema"
    {
        return Some(CannedResponse::single_text("current_schema", "public"));
    }
    if lower == "select current_user" || lower == "select user" {
        return Some(CannedResponse::single_text("current_user", "admin"));
    }
    // ---- v4.15 pgbouncer compat: connection-reset statements ----
    // pgbouncer issues these between pooled client sessions to
    // wipe per-connection state. SPG doesn't have per-connection
    // settings worth wiping, so all are no-ops.
    if lower.starts_with("discard all") {
        return Some(CannedResponse::Tag("DISCARD ALL"));
    }
    if lower.starts_with("discard temp")
        || lower.starts_with("discard sequences")
        || lower.starts_with("discard plans")
    {
        return Some(CannedResponse::Tag("DISCARD"));
    }
    if lower == "reset all" || lower.starts_with("reset ") {
        return Some(CannedResponse::Tag("RESET"));
    }
    // v4.18: VACUUM / ANALYZE / CLUSTER / REINDEX — BI clients
    // (Metabase, DBeaver) run these defensively after schema
    // changes. SPG has no vacuum or analyze concept (rows are
    // dense; no MVCC dead tuples; index stats aren't sampled),
    // so they're all no-ops.
    if lower.starts_with("vacuum") {
        return Some(CannedResponse::Tag("VACUUM"));
    }
    if lower.starts_with("analyze") {
        return Some(CannedResponse::Tag("ANALYZE"));
    }
    if lower.starts_with("cluster") {
        return Some(CannedResponse::Tag("CLUSTER"));
    }
    if lower.starts_with("reindex") {
        return Some(CannedResponse::Tag("REINDEX"));
    }
    // BEGIN ISOLATION LEVEL READ COMMITTED / SERIALIZABLE etc. —
    // pgbouncer + ORMs often prefix transactions with a level.
    // SPG only has one isolation level; accept the syntactic
    // variants without disturbing the engine. Real BEGIN dispatch
    // happens through the normal engine path when it's a bare
    // BEGIN / START TRANSACTION (no isolation specifier).
    if lower.starts_with("begin isolation level")
        || lower.starts_with("begin transaction isolation level")
        || lower.starts_with("start transaction isolation level")
        || lower.starts_with("set transaction isolation level")
        || lower.starts_with("set transaction read")
        || lower.starts_with("set transaction snapshot")
    {
        // BEGIN-ish variants need to actually open a TX in the
        // engine. Fall through to the regular path by returning
        // None; the engine ignores trailing modifiers in BEGIN.
        // SET TRANSACTION is purely informational — no-op tag.
        if lower.starts_with("set transaction") {
            return Some(CannedResponse::Tag("SET"));
        }
    }
    // ---- v4.6 pg_catalog subset ----
    if mentions_pg_table(&lower, "pg_class") {
        return Some(pg_class_response(state));
    }
    if mentions_pg_table(&lower, "pg_namespace") {
        return Some(pg_namespace_response());
    }
    if mentions_pg_table(&lower, "pg_database") {
        return Some(pg_database_response());
    }
    if mentions_pg_table(&lower, "pg_user") || mentions_pg_table(&lower, "pg_roles") {
        return Some(pg_user_response(state));
    }
    if mentions_pg_table(&lower, "pg_tables") {
        // The convenience view PG ships; columns: schemaname, tablename, tableowner.
        return Some(pg_tables_response(state));
    }
    None
}

/// True when `sql_lower` references the given pg_catalog table name,
/// either bare (`pg_class`) or schema-qualified (`pg_catalog.pg_class`).
/// Used to dispatch the canned synthesizer; intentionally permissive —
/// any false-positive just gets a synthesized response with all rows,
/// and the client filters client-side.
fn mentions_pg_table(sql_lower: &str, table: &str) -> bool {
    sql_lower.contains(&format!("from {table}"))
        || sql_lower.contains(&format!("from pg_catalog.{table}"))
        || sql_lower.contains(&format!("join {table}"))
        || sql_lower.contains(&format!("join pg_catalog.{table}"))
}

enum CannedResponse {
    Rows {
        columns: Vec<ColumnSchema>,
        rows: Vec<Row>,
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
            rows: vec![Row::new(vec![Value::Text(val.to_string())])],
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

fn pg_class_response(state: &Arc<ServerState>) -> CannedResponse {
    // Canonical-ish pg_class columns. We expose just oid / relname /
    // relkind / relnamespace / relowner — the columns most simple
    // clients project. SPG only has user tables (kind `r`) — no
    // indexes/sequences/views in the pg_catalog sense.
    let columns = vec![
        ColumnSchema::new("oid", DataType::BigInt, false),
        ColumnSchema::new("relname", DataType::Text, false),
        ColumnSchema::new("relkind", DataType::Text, false),
        ColumnSchema::new("relnamespace", DataType::BigInt, false),
        ColumnSchema::new("relowner", DataType::BigInt, false),
    ];
    let rows = state
        .engine
        .read()
        .map(|e| {
            e.catalog()
                .table_names()
                .into_iter()
                .enumerate()
                .map(|(i, name)| {
                    Row::new(vec![
                        Value::BigInt(16384 + i as i64), // synthetic oid (PG user oids start ~16384)
                        Value::Text(name),
                        Value::Text("r".to_string()),
                        Value::BigInt(2200), // public schema oid
                        Value::BigInt(10),   // owner oid (synthetic admin)
                    ])
                })
                .collect()
        })
        .unwrap_or_default();
    CannedResponse::Rows { columns, rows }
}

fn pg_namespace_response() -> CannedResponse {
    let columns = vec![
        ColumnSchema::new("oid", DataType::BigInt, false),
        ColumnSchema::new("nspname", DataType::Text, false),
        ColumnSchema::new("nspowner", DataType::BigInt, false),
    ];
    let rows = vec![Row::new(vec![
        Value::BigInt(2200),
        Value::Text("public".to_string()),
        Value::BigInt(10),
    ])];
    CannedResponse::Rows { columns, rows }
}

fn pg_database_response() -> CannedResponse {
    let columns = vec![
        ColumnSchema::new("oid", DataType::BigInt, false),
        ColumnSchema::new("datname", DataType::Text, false),
        ColumnSchema::new("datdba", DataType::BigInt, false),
    ];
    let rows = vec![Row::new(vec![
        Value::BigInt(16384),
        Value::Text("spg".to_string()),
        Value::BigInt(10),
    ])];
    CannedResponse::Rows { columns, rows }
}

fn pg_user_response(state: &Arc<ServerState>) -> CannedResponse {
    let columns = vec![
        ColumnSchema::new("usename", DataType::Text, false),
        ColumnSchema::new("usesuper", DataType::Bool, false),
    ];
    let rows = state
        .engine
        .read()
        .map(|e| {
            if e.users().is_empty() {
                vec![Row::new(vec![
                    Value::Text("admin".to_string()),
                    Value::Bool(true),
                ])]
            } else {
                e.users()
                    .iter()
                    .map(|(name, rec)| {
                        Row::new(vec![
                            Value::Text(name.to_string()),
                            Value::Bool(matches!(rec.role, spg_engine::Role::Admin)),
                        ])
                    })
                    .collect()
            }
        })
        .unwrap_or_default();
    CannedResponse::Rows { columns, rows }
}

fn pg_tables_response(state: &Arc<ServerState>) -> CannedResponse {
    let columns = vec![
        ColumnSchema::new("schemaname", DataType::Text, false),
        ColumnSchema::new("tablename", DataType::Text, false),
        ColumnSchema::new("tableowner", DataType::Text, false),
    ];
    let rows = state
        .engine
        .read()
        .map(|e| {
            e.catalog()
                .table_names()
                .into_iter()
                .map(|name| {
                    Row::new(vec![
                        Value::Text("public".to_string()),
                        Value::Text(name),
                        Value::Text("admin".to_string()),
                    ])
                })
                .collect()
        })
        .unwrap_or_default();
    CannedResponse::Rows { columns, rows }
}

// ---- v4.7 extended-query protocol ----
//
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
    params: Vec<spg_storage::Value>,
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
    drop(eng);
    let placeholder_count = count_placeholders(&sql);
    prepared.insert(
        name,
        PreparedStmt {
            ast,
            placeholder_count,
            param_type_oids,
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
    let mut params: Vec<spg_storage::Value> = Vec::with_capacity(param_count);
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
    // Trailing result-format-codes — we always return text on the
    // wire, so ignore them here.
    Ok((portal_name, Portal { stmt_name, params }))
}

/// v6.1.1 — convert a pgwire text-format bind parameter into a
/// typed `Value`. Numeric / boolean tokens narrow to the matching
/// scalar so the engine sees a `Literal::Integer(123)` rather
/// than `Literal::String("123")` after the substitute walk (which
/// would then fail to compare against an INT column without an
/// explicit cast). The narrowing is conservative: only inputs
/// that round-trip cleanly to text get the typed treatment.
fn text_param_to_value(s: &str) -> spg_storage::Value {
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
        return spg_storage::Value::Vector(v);
    }
    spg_storage::Value::Text(s.to_string())
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
fn decode_binary_param(oid: u32, bytes: &[u8]) -> Result<spg_storage::Value, String> {
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
                Ok(Value::Text(if s.is_empty() { "\\x".into() } else { s }))
            } else {
                let s = std::str::from_utf8(bytes)
                    .map_err(|_| "Bind binary TEXT/VARCHAR: invalid UTF-8".to_string())?;
                Ok(Value::Text(s.to_string()))
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
fn decode_binary_numeric(bytes: &[u8]) -> Result<spg_storage::Value, String> {
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
    // dscale fits in u8; precision is best-effort (38 = i128 max).
    let scale = u8::try_from(dscale).map_err(|_| "NUMERIC dscale too large".to_string())?;
    Ok(spg_storage::Value::Numeric {
        scaled: final_value,
        scale,
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
    portals: &std::collections::HashMap<String, Portal>,
    prepared: &std::collections::HashMap<String, PreparedStmt>,
    settings: &std::collections::HashMap<String, String>,
    stream: &mut dyn Write,
    state: &Arc<ServerState>,
    role: Role,
    tx_state: &mut u8,
) -> Result<(), (&'static str, String)> {
    // v7.17.0 Phase 2.3 — protocol-level errors keep SQLSTATE
    // `42000` to match prior behavior; only `EngineError::Cancelled`
    // promotes to `57014` via `engine_error_to_wire`.
    let proto = |m: String| ("42000", m);
    let mut cur = 0;
    let portal_name = read_cstring(body, &mut cur)
        .ok_or_else(|| proto("Execute: portal name not UTF-8".to_string()))?
        .to_string();
    // Max-rows (i32, 0 = unlimited). We always return everything;
    // partial-cursor support is future work.
    if cur + 4 > body.len() {
        return Err(proto("Execute: missing max-rows".to_string()));
    }
    let portal = portals
        .get(&portal_name)
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
    let cancel = statement_cancel(settings);
    // v6.1.1: dispatch through `Engine::execute_prepared` — the
    // AST is reused from Parse; only the substitute walk + dispatch
    // happen here. No SQL re-parse, no canned-response check (the
    // canned probes are simple-query shape only; an ORM that
    // PREPAREs `SELECT version()` doesn't need the canned path
    // because the engine itself will satisfy it).
    let needs_write = !matches!(&stmt.ast, spg_sql::ast::Statement::Select(_));
    let result = {
        let mut eng = if needs_write {
            // Same lock-tier shape as `execute_with_role`'s
            // write path. We hold `engine.write()` for the
            // duration of execute_prepared so transactional
            // state (in_transaction, savepoints) stays
            // single-writer.
            state
                .engine
                .write()
                .map_err(|_| proto("Execute: engine lock poisoned".to_string()))?
        } else {
            // SELECT-only path could take a read lock today —
            // but execute_prepared takes &mut self for symmetry
            // with the simple-query path. Leave the write lock
            // for the v6.1.1 commit; v6.2 can introduce a
            // dedicated read-only execute_prepared_readonly()
            // and avoid the upgrade. Hot kNN reads still benefit
            // from the parse caching, which is the load-bearing
            // win.
            state
                .engine
                .write()
                .map_err(|_| proto("Execute: engine lock poisoned".to_string()))?
        };
        // Role gate — same shape as `execute_with_role`.
        if needs_write && matches!(role, Role::ReadOnly) {
            return Err(proto("permission denied: readonly role".to_string()));
        }
        eng.execute_prepared_with_cancel(stmt.ast.clone(), &portal.params, cancel)
    };
    match result {
        Ok(QueryResult::Rows { columns, rows }) => {
            send_row_description(stream, &columns).map_err(|e| proto(e.to_string()))?;
            let n = rows.len();
            for row in &rows {
                send_data_row(stream, &columns, row).map_err(|e| proto(e.to_string()))?;
            }
            send_command_complete(stream, &format!("SELECT {n}"))
                .map_err(|e| proto(e.to_string()))?;
        }
        Ok(QueryResult::CommandOk { affected, .. }) => {
            // Synthesise a command tag from the statement kind so
            // drivers see e.g. "INSERT 0 1" rather than the
            // simple-query path's per-SQL synthesis. We re-derive
            // the tag from the AST root, not the original SQL
            // text — text is owned by Parse, not Execute.
            let tag = command_tag_for_ast(&stmt.ast, affected);
            send_command_complete(stream, &tag).map_err(|e| proto(e.to_string()))?;
            *tx_state = if state.engine.read().is_ok_and(|e| e.in_transaction()) {
                b'T'
            } else {
                b'I'
            };
        }
        Err(e) => {
            let (sqlstate, msg) = engine_error_to_wire(&e);
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
fn command_tag_for_ast(stmt: &spg_sql::ast::Statement, affected: usize) -> String {
    use spg_sql::ast::Statement;
    match stmt {
        Statement::Insert(_) => format!("INSERT 0 {affected}"),
        Statement::Update(_) => format!("UPDATE {affected}"),
        Statement::Delete(_) => format!("DELETE {affected}"),
        Statement::CreateTable(_) => "CREATE TABLE".to_string(),
        Statement::CreateIndex(_) => "CREATE INDEX".to_string(),
        Statement::AlterIndex(_) => "ALTER INDEX".to_string(),
        Statement::Begin => "BEGIN".to_string(),
        Statement::Commit => "COMMIT".to_string(),
        Statement::Rollback => "ROLLBACK".to_string(),
        Statement::Savepoint(_) => "SAVEPOINT".to_string(),
        Statement::RollbackToSavepoint(_) => "ROLLBACK".to_string(),
        Statement::ReleaseSavepoint(_) => "RELEASE".to_string(),
        Statement::CreateUser(_) => "CREATE USER".to_string(),
        Statement::DropUser(_) => "DROP USER".to_string(),
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
    let lower = trimmed.to_ascii_lowercase();
    let rest = lower.strip_prefix("set ")?;
    // Strip optional SESSION / LOCAL.
    let rest = rest
        .strip_prefix("session ")
        .or_else(|| rest.strip_prefix("local "))
        .unwrap_or(rest);
    // Find name + assign operator (= or TO).
    let (name, value_part) = if let Some(idx) = rest.find('=') {
        (rest[..idx].trim().to_string(), rest[idx + 1..].trim())
    } else if let Some(idx) = rest.find(" to ") {
        (rest[..idx].trim().to_string(), rest[idx + 4..].trim())
    } else {
        return None;
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
    let lower = sql.trim().to_ascii_lowercase();
    let rest = lower.strip_prefix("show ")?;
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
        let rows: Vec<Row> = entries
            .into_iter()
            .map(|(n, v)| Row::new(vec![Value::Text(n), Value::Text(v), Value::Null]))
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
        rows: vec![Row::new(vec![Value::Text(value)])],
    }
}

/// Built-in PG GUCs we report sane defaults for so clients
/// configuring themselves at startup don't get an empty SHOW.
fn known_defaults() -> &'static [(&'static str, &'static str)] {
    &[
        ("application_name", ""),
        ("client_encoding", "UTF8"),
        ("datestyle", "ISO, MDY"),
        ("default_transaction_isolation", "read committed"),
        ("default_transaction_read_only", "off"),
        ("intervalstyle", "postgres"),
        ("search_path", "\"$user\", public"),
        ("server_encoding", "UTF8"),
        ("server_version", "16.0 (spg-4.19)"),
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
fn statement_cancel(settings: &std::collections::HashMap<String, String>) -> CancelToken<'static> {
    let raw = settings
        .get("statement_timeout")
        .map(String::as_str)
        .unwrap_or("0");
    let Some(ms) = parse_timeout_ms(raw) else {
        return CancelToken::none();
    };
    if ms == 0 {
        return CancelToken::none();
    }
    let now_fn: MonotonicNowFn = monotonic_now_us;
    let deadline_us = monotonic_now_us().saturating_add(ms.saturating_mul(1_000));
    CancelToken::none().with_deadline(now_fn, deadline_us)
}

/// Map an `EngineError` to (SQLSTATE, wire message). v7.17.0
/// Phase 2.3 — separates `Cancelled` so it lands as PG's standard
/// `57014` (`query_canceled`) with the canonical "canceling
/// statement due to statement timeout" text driver libraries grep
/// for. Everything else stays on the legacy `42000` to preserve
/// existing client-side error parsing.
fn engine_error_to_wire(e: &EngineError) -> (&'static str, String) {
    match e {
        EngineError::Cancelled => (
            "57014",
            "canceling statement due to statement timeout".to_string(),
        ),
        _ => ("42000", e.to_string()),
    }
}

// ---- v4.17 COPY FROM STDIN / COPY TO STDOUT ----

#[derive(Debug)]
enum CopyIntent {
    From(String, CopyOptions),
    To(String),
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
    let lower = trimmed.to_ascii_lowercase();
    let rest = lower.strip_prefix("copy ")?;
    // Walk the prefix manually so we can skip an optional
    // parenthesised column list between the table name and the
    // FROM/TO keyword. Token splitting on whitespace alone
    // mistakes `(col1,` for the FROM direction word.
    let bytes = rest.as_bytes();
    let mut i = skip_ws_bytes(bytes, 0);
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
    if bytes.get(i) == Some(&b'(') {
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
    match (dir, endpoint) {
        ("from", "stdin") => {
            // Look for the WITH (...) tail and parse options.
            let opts = parse_copy_options(&lower);
            Some(CopyIntent::From(table, opts))
        }
        ("to", "stdout") => Some(CopyIntent::To(table)),
        _ => None,
    }
}

fn skip_ws_bytes(bytes: &[u8], mut i: usize) -> usize {
    while i < bytes.len() && (bytes[i] as char).is_ascii_whitespace() {
        i += 1;
    }
    i
}

/// Find a `WITH (...)` chunk in the SQL and decode the options.
fn parse_copy_options(lower: &str) -> CopyOptions {
    let mut opts = CopyOptions::default();
    let Some(open) = lower.find('(') else {
        return opts;
    };
    let Some(close) = lower[open..].find(')') else {
        return opts;
    };
    let inner = &lower[open + 1..open + close];
    for pair in inner.split(',') {
        let pair = pair.trim();
        if pair.is_empty() {
            continue;
        }
        let mut it = pair.split_ascii_whitespace();
        let key = it.next().unwrap_or("");
        let val = it.next().unwrap_or("");
        match key {
            "skip" => {
                opts.skip = val.parse().unwrap_or(0);
            }
            "on_error" => {
                if val == "set_null" {
                    opts.on_error_set_null = true;
                }
            }
            "format" => {
                if val == "json" {
                    opts.format_json = true;
                }
            }
            _ => {}
        }
    }
    opts
}

/// COPY FROM STDIN — server sends CopyInResponse, reads CopyData
/// frames, parses each row (tab-delimited text, `\N` = NULL),
/// inserts via engine.execute("INSERT ..."). CopyDone commits;
/// CopyFail aborts.
fn handle_copy_from_stdin(
    stream: &mut TcpStream,
    state: &Arc<ServerState>,
    role: Role,
    table: &str,
    opts: &CopyOptions,
    tx_state: &mut u8,
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
        if let Err(msg) =
            process_copy_chunk(state, table, &mut buf, &mut inserted, &mut skipped, opts)
        {
            send_error(stream, "22P02", &msg)?;
            return Ok(());
        }
    }
    // Final drain.
    if let Err(msg) = process_copy_chunk(state, table, &mut buf, &mut inserted, &mut skipped, opts)
    {
        send_error(stream, "22P02", &msg)?;
        return Ok(());
    }
    send_command_complete(stream, &format!("COPY {inserted}"))?;
    *tx_state = if state.engine.read().is_ok_and(|e| e.in_transaction()) {
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
    buf: &mut Vec<u8>,
    inserted: &mut u64,
    skipped: &mut u64,
    opts: &CopyOptions,
) -> Result<(), String> {
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
            build_copy_insert(table, &values)
        };
        let mut engine = state
            .engine
            .write()
            .map_err(|_| "engine rwlock poisoned".to_string())?;
        match engine.execute(&sql) {
            Ok(_) => *inserted += 1,
            Err(e) => {
                if opts.on_error_set_null {
                    // Best-effort: skip the row but keep going.
                    continue;
                }
                return Err(format!("COPY row INSERT failed: {e}"));
            }
        }
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

/// COPY TO STDOUT — server runs `SELECT * FROM <table>`, sends
/// CopyOutResponse, streams each row as one CopyData frame (text
/// format), then CopyDone + CommandComplete.
fn handle_copy_to_stdout(
    stream: &mut TcpStream,
    state: &Arc<ServerState>,
    role: Role,
    table: &str,
    tx_state: &mut u8,
) -> std::io::Result<()> {
    let _ = role.can_read(); // every role can read
    let sql = format!("SELECT * FROM {table}");
    // v7.17.0 Phase 2.3 — COPY TO STDOUT does not honor
    // `statement_timeout` in this phase (the existing settings map
    // does not cross the COPY boundary); bulk-export paths get
    // `CancelToken::none()`. SPG_QUERY_TIMEOUT_MS / server-wide cap
    // still applies via the server-state watchdog.
    let result = execute_with_role(state, &sql, role, CancelToken::none());
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
    for row in &rows {
        let mut line = String::new();
        for (i, v) in row.values.iter().enumerate() {
            if i > 0 {
                line.push('\t');
            }
            line.push_str(&encode_copy_cell(v));
        }
        line.push('\n');
        send_msg(stream, b'd', line.as_bytes())?;
    }
    send_msg(stream, b'c', &[])?; // CopyDone
    send_command_complete(stream, &format!("COPY {n}"))?;
    // No TX state change.
    let _ = tx_state;
    Ok(())
}

/// Inverse of decode_copy_text_row's cell decode: render a Value
/// as a tab-safe text cell with `\N` for NULL.
fn encode_copy_cell(v: &spg_storage::Value) -> String {
    use spg_storage::Value;
    match v {
        Value::Null => "\\N".to_string(),
        Value::Bool(b) => if *b { "t" } else { "f" }.to_string(),
        Value::SmallInt(n) => n.to_string(),
        Value::Int(n) => n.to_string(),
        Value::BigInt(n) => n.to_string(),
        Value::Float(x) => format!("{x}"),
        Value::Text(s) | Value::Json(s) => escape_copy_cell(s),
        Value::Numeric { scaled, scale } => spg_engine::eval::format_numeric(*scaled, *scale),
        Value::Date(d) => spg_engine::eval::format_date(*d),
        Value::Timestamp(t) => spg_engine::eval::format_timestamp(*t),
        Value::Interval { months, micros } => spg_engine::eval::format_interval(*months, *micros),
        Value::Vector(v) => {
            let parts: Vec<String> = v.iter().map(std::string::ToString::to_string).collect();
            escape_copy_cell(&format!("[{}]", parts.join(", ")))
        }
        // v6.0.1: COPY OUT a `VECTOR(N) USING SQ8` column —
        // dequantise to f32 so the COPY text stream stays
        // pgvector-compatible.
        Value::Sq8Vector(q) => {
            let parts: Vec<String> = spg_storage::quantize::dequantize(q)
                .iter()
                .map(std::string::ToString::to_string)
                .collect();
            escape_copy_cell(&format!("[{}]", parts.join(", ")))
        }
        // v6.0.3: COPY OUT for `VECTOR(N) USING HALF` — bit-exact
        // dequantise to f32.
        Value::HalfVector(h) => {
            let parts: Vec<String> = h
                .to_f32_vec()
                .iter()
                .map(std::string::ToString::to_string)
                .collect();
            escape_copy_cell(&format!("[{}]", parts.join(", ")))
        }
        // v7.5.0 — Value is #[non_exhaustive].
        _ => escape_copy_cell(&format!("{v:?}")),
    }
}

/// Escape a text cell per PG COPY text-format rules: backslash,
/// tab, newline, CR, and a literal `\b`/`\f`/`\v` get the standard
/// escape sequences.
fn escape_copy_cell(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            '\u{0b}' => out.push_str("\\v"),
            c => out.push(c),
        }
    }
    out
}

// ---- Auth helpers (cleartext + SCRAM) ----

fn cleartext_auth(
    stream: &mut TcpStream,
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
    stream: &mut TcpStream,
    state: &Arc<ServerState>,
    user: &str,
) -> std::io::Result<Option<Role>> {
    // ---- Step 1: AuthenticationSASL ----
    // Mechanism list is null-terminated mechanism strings, ended by
    // an empty string (= double null).
    let mut sasl_body = Vec::new();
    sasl_body.extend_from_slice(&10u32.to_be_bytes());
    sasl_body.extend_from_slice(b"SCRAM-SHA-256\0\0");
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
    if mech != "SCRAM-SHA-256" {
        send_error(
            stream,
            "28000",
            &format!("only SCRAM-SHA-256 is supported, got {mech:?}"),
        )?;
        return Ok(None);
    }
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

fn read_startup(stream: &mut TcpStream) -> std::io::Result<(String, Vec<(String, String)>)> {
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

fn read_password_message(stream: &mut TcpStream) -> std::io::Result<String> {
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

fn send_command_complete(stream: &mut dyn Write, tag: &str) -> std::io::Result<()> {
    let mut body = Vec::with_capacity(tag.len() + 1);
    body.extend_from_slice(tag.as_bytes());
    body.push(0);
    send_msg(stream, b'C', &body)
}

fn send_error(stream: &mut dyn Write, sqlstate: &str, msg: &str) -> std::io::Result<()> {
    // ErrorResponse: each field is `[fieldcode byte][value][\0]`,
    // terminated by a single `\0`. Minimum useful set: S (severity),
    // C (sqlstate), M (message).
    let mut body = Vec::new();
    body.push(b'S');
    body.extend_from_slice(b"ERROR");
    body.push(0);
    body.push(b'C');
    body.extend_from_slice(sqlstate.as_bytes());
    body.push(0);
    body.push(b'M');
    body.extend_from_slice(msg.as_bytes());
    body.push(0);
    body.push(0);
    send_msg(stream, b'E', &body)
}

fn send_row_description(stream: &mut dyn Write, cols: &[ColumnSchema]) -> std::io::Result<()> {
    let n = u16::try_from(cols.len())
        .map_err(|_| std::io::Error::other("RowDescription: too many columns"))?;
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
    send_msg(stream, b'T', &body)
}

fn send_data_row(stream: &mut dyn Write, cols: &[ColumnSchema], row: &Row) -> std::io::Result<()> {
    let n = u16::try_from(row.values.len())
        .map_err(|_| std::io::Error::other("DataRow: too many cells"))?;
    let mut body = Vec::with_capacity(2 + row.values.len() * 8);
    body.extend_from_slice(&n.to_be_bytes());
    for (i, v) in row.values.iter().enumerate() {
        let text = value_to_pg_text(v, cols.get(i).map(|c| c.ty));
        match text {
            None => body.extend_from_slice(&(-1i32).to_be_bytes()), // NULL
            Some(s) => {
                let len = i32::try_from(s.len())
                    .map_err(|_| std::io::Error::other("cell value too large"))?;
                body.extend_from_slice(&len.to_be_bytes());
                body.extend_from_slice(s.as_bytes());
            }
        }
    }
    send_msg(stream, b'D', &body)
}

// ---- Type mapping ----

/// PG type OIDs lifted from postgres `src/include/catalog/pg_type.dat`.
/// Catch-all is `text` (25) so an unknown / new SPG type round-trips
/// as a readable string rather than confusing the client.
const fn pg_type_oid(ty: DataType) -> u32 {
    match ty {
        DataType::Bool => 16,
        DataType::SmallInt => 21,
        DataType::Int => 23,
        DataType::BigInt => 20,
        DataType::Float => 701,
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
    }
}

const fn pg_type_len(ty: DataType) -> i16 {
    match ty {
        DataType::Bool => 1,
        DataType::SmallInt => 2,
        DataType::Int | DataType::Date => 4,
        DataType::BigInt | DataType::Float | DataType::Timestamp => 8,
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

fn value_to_pg_text(v: &Value, ty: Option<DataType>) -> Option<String> {
    Some(match v {
        Value::Null => return None,
        Value::Bool(b) => if *b { "t" } else { "f" }.to_string(),
        Value::SmallInt(n) => n.to_string(),
        Value::Int(n) => n.to_string(),
        Value::BigInt(n) => n.to_string(),
        Value::Float(f) => format!("{f}"),
        Value::Text(s) | Value::Json(s) => s.clone(),
        // v7.15.0 — TIMESTAMPTZ vs plain TIMESTAMP at render
        // time. mailrs round-8 acceptance: SELECT on TIMESTAMPTZ
        // must round-trip to a literal pg_dump would emit (i.e.
        // include the `+00` UTC offset).
        Value::Timestamp(micros) if matches!(ty, Some(DataType::Timestamptz)) => {
            spg_engine::eval::format_timestamptz(*micros)
        }
        Value::Timestamp(micros) => format_timestamp(*micros),
        Value::Date(days) => format_date(*days),
        Value::Interval { months, micros } => format!("P{months}M{micros}U"),
        Value::Numeric { scaled, scale } => format_numeric(*scaled, *scale),
        Value::Vector(v) => {
            let parts: Vec<String> = v.iter().map(std::string::ToString::to_string).collect();
            format!("[{}]", parts.join(", "))
        }
        // v6.0.1: pgwire text-format render for SQ8 cells —
        // dequantise so clients see the pgvector-style
        // `[x, y, z]` payload.
        Value::Sq8Vector(q) => {
            let parts: Vec<String> = spg_storage::quantize::dequantize(q)
                .iter()
                .map(std::string::ToString::to_string)
                .collect();
            format!("[{}]", parts.join(", "))
        }
        // v6.0.3: pgwire text-format render for HALF cells.
        Value::HalfVector(h) => {
            let parts: Vec<String> = h
                .to_f32_vec()
                .iter()
                .map(std::string::ToString::to_string)
                .collect();
            format!("[{}]", parts.join(", "))
        }
        // v7.17.0 — UUID renders canonical 8-4-4-4-12 lowercase
        // hyphenated. Matches PG `uuid_out` so libpq clients,
        // psql `\d`, and sqlx text-mode decoders all read the
        // standard form.
        Value::Uuid(b) => spg_storage::format_uuid(b),
        // v7.17.0 Phase 3.P0-32 — TIME renders via the shared
        // engine helper so the trim-trailing-zeros shape matches
        // PG `time_out` across pgwire, sqllogictest, and engine.
        Value::Time(us) => spg_engine::eval::format_time(*us),
        // v7.17.0 Phase 3.P0-33 — YEAR renders 4-digit zero-padded.
        Value::Year(y) => format!("{y:04}"),
        // v7.17.0 Phase 3.P0-34 — TIMETZ via the shared engine
        // helper so the canonical `HH:MM:SS[.ffffff]±HH[:MM]`
        // shape matches PG `timetz_out` across all renderers.
        Value::TimeTz { us, offset_secs } => spg_engine::eval::format_timetz(*us, *offset_secs),
        // v7.17.0 Phase 3.P0-35 — MONEY via the shared engine
        // helper so the canonical en_US text form matches PG
        // `cash_out` across all renderers.
        Value::Money(c) => spg_engine::eval::format_money(*c),
        // v7.17.0 Phase 3.P0-38 — Range via shared engine helper.
        Value::Range { .. } => spg_engine::format_range_text(v),
        // v7.17.0 Phase 3.P0-39 — Hstore via shared engine helper.
        Value::Hstore(pairs) => spg_engine::format_hstore_text(pairs),
        // v7.17.0 Phase 3.P0-40 — 2D arrays via shared helpers.
        Value::IntArray2D(rows) => spg_engine::format_int_2d_text_pub(rows),
        Value::BigIntArray2D(rows) => spg_engine::format_bigint_2d_text_pub(rows),
        Value::TextArray2D(rows) => spg_engine::format_text_2d_text_pub(rows),
        // v7.5.0 — Value is #[non_exhaustive].
        _ => format!("{v:?}"),
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

fn format_numeric(scaled: i128, scale: u8) -> String {
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

    /// v7.15.0 — pg_dump's default output emits `COPY t (col, col)
    /// FROM stdin;` for every table with data. The old whitespace-
    /// split parser mistook `(col,` for the FROM direction word and
    /// missed the COPY entirely; the new bytes-walk skips the
    /// parenthesised column list.
    #[test]
    fn parse_copy_with_column_list() {
        let sql = "COPY posts (id, title, body) FROM stdin;";
        match parse_copy_intent(sql) {
            Some(CopyIntent::From(table, _)) => assert_eq!(table, "posts"),
            other => panic!("expected From(posts), got {other:?}"),
        }
    }

    #[test]
    fn parse_copy_without_column_list() {
        let sql = "COPY accounts FROM STDIN";
        match parse_copy_intent(sql) {
            Some(CopyIntent::From(table, _)) => assert_eq!(table, "accounts"),
            other => panic!("expected From(accounts), got {other:?}"),
        }
    }

    #[test]
    fn parse_copy_to_stdout_with_column_list() {
        let sql = "COPY t (a, b) TO STDOUT";
        match parse_copy_intent(sql) {
            Some(CopyIntent::To(table)) => assert_eq!(table, "t"),
            other => panic!("expected To(t), got {other:?}"),
        }
    }

    #[test]
    fn parse_copy_with_with_options() {
        let sql = "COPY t FROM stdin WITH (format json)";
        match parse_copy_intent(sql) {
            Some(CopyIntent::From(table, opts)) => {
                assert_eq!(table, "t");
                assert!(opts.format_json);
            }
            other => panic!("expected From(t) with format_json, got {other:?}"),
        }
    }

    #[test]
    fn parse_non_copy_returns_none() {
        assert!(parse_copy_intent("SELECT 1").is_none());
        // File-based COPY: pgwire intentionally doesn't handle.
        assert!(parse_copy_intent("COPY t FROM '/etc/passwd'").is_none());
    }
}
