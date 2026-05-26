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

use spg_engine::{EngineError, QueryResult, Role};
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
    let _ = params; // database / options / etc. — we only honor `user`
    // RBAC: if there are users in the engine, demand password.
    // Else (open mode), accept any startup as admin.
    let has_users = state.engine.read().is_ok_and(|e| !e.users().is_empty());

    let role = if has_users {
        // AuthenticationCleartextPassword
        send_msg(&mut stream, b'R', &3u32.to_be_bytes())?;
        let pwd = read_password_message(&mut stream)?;
        let verified = state
            .engine
            .read()
            .ok()
            .and_then(|e| e.verify_user(&user, &pwd));
        if let Some(r) = verified {
            r
        } else {
            send_error(&mut stream, "28P01", "password authentication failed")?;
            return Ok(());
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
                let Ok(sql_str) = std::str::from_utf8(sql_bytes) else {
                    send_error(&mut stream, "22021", "invalid UTF-8 in query")?;
                    send_ready_for_query(&mut stream, tx_state)?;
                    continue;
                };
                let sql = sql_str.trim_end_matches(';').trim().to_string();
                // psql sends startup probes like "SELECT version()" /
                // "SHOW search_path". Stub the common ones with sane
                // canned answers so the client doesn't error out.
                if let Some(canned) = canned_response(&sql, state) {
                    send_canned(&mut stream, &canned)?;
                    send_ready_for_query(&mut stream, tx_state)?;
                    continue;
                }
                let result = execute_with_role(state, &sql, role);
                match result {
                    Ok(QueryResult::Rows { columns, rows }) => {
                        send_row_description(&mut stream, &columns)?;
                        let n = rows.len();
                        for row in &rows {
                            send_data_row(&mut stream, &columns, row)?;
                        }
                        send_command_complete(&mut stream, &format!("SELECT {n}"))?;
                    }
                    Ok(QueryResult::CommandOk { affected, .. }) => {
                        let tag = command_tag(&sql, affected);
                        send_command_complete(&mut stream, &tag)?;
                        // Sync tx state from engine after writes.
                        tx_state = if state.engine.read().is_ok_and(|e| e.in_transaction()) {
                            b'T'
                        } else {
                            b'I'
                        };
                    }
                    Err(e) => {
                        send_error(&mut stream, "42000", &e.to_string())?;
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
                }
                send_ready_for_query(&mut stream, tx_state)?;
            }
            b'X' => return Ok(()),
            // ---- v4.7: extended-query protocol ----
            // Parse (P): name + SQL + parameter type OIDs. Store the
            // statement; reply ParseComplete (no ReadyForQuery — that
            // waits for Sync).
            b'P' => {
                if let Err(msg) = handle_parse(&body, &mut prepared) {
                    send_error(&mut stream, "42601", &msg)?;
                } else {
                    send_msg(&mut stream, b'1', &[])?;
                }
            }
            // Bind (B): create a portal with parameter values
            // substituted into the prepared statement's SQL.
            b'B' => {
                match handle_bind(&body, &prepared) {
                    Ok(portal) => {
                        portals.insert(portal.0.clone(), portal.1);
                        send_msg(&mut stream, b'2', &[])?; // BindComplete
                    }
                    Err(msg) => send_error(&mut stream, "42601", &msg)?,
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
                    // PG spec says NoData (`n`) is the right reply
                    // when we can't compute the row description ahead
                    // of time. Most drivers tolerate this even though
                    // it forces them to read RD off the Execute reply.
                    if kind == b'S' {
                        // ParameterDescription: zero parameters
                        // declared (we'll trust the Bind to count them).
                        let mut pd = Vec::with_capacity(2);
                        pd.extend_from_slice(&0u16.to_be_bytes());
                        send_msg(&mut stream, b't', &pd)?;
                    }
                    send_msg(&mut stream, b'n', &[])?; // NoData
                }
            }
            // Execute (E): portal name + max-rows (0 = all).
            b'E' => {
                if let Err(msg) =
                    handle_execute(&body, &portals, &mut stream, state, role, &mut tx_state)
                {
                    send_error(&mut stream, "42000", &msg)?;
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
                send_msg(&mut stream, b'3', &[])?; // CloseComplete
            }
            // Flush (H): no-op for our buffered TcpStream (everything
            // is already on the wire after each send_msg). Spec says
            // no reply is required.
            b'H' => {}
            // Sync (S): boundary marker — reply with ReadyForQuery
            // reflecting the current transaction state.
            b'S' => {
                send_ready_for_query(&mut stream, tx_state)?;
            }
            // CopyData (d), CopyDone (c), CopyFail (f) — we don't
            // support COPY at all, but reject cleanly.
            b'd' | b'c' | b'f' => {
                send_error(&mut stream, "0A000", "COPY protocol not supported")?;
                send_ready_for_query(&mut stream, tx_state)?;
            }
            _ => {
                send_error(
                    &mut stream,
                    "08P01",
                    &format!("unknown frontend message type: 0x{msg_type:02x}"),
                )?;
                send_ready_for_query(&mut stream, tx_state)?;
            }
        }
    }
}

fn execute_with_role(
    state: &Arc<ServerState>,
    sql: &str,
    role: Role,
) -> Result<QueryResult, EngineError> {
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
    let is_read = matches!(lower_first.as_str(), "select" | "show");
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
        engine.execute_readonly(sql)
    } else {
        let mut engine = state
            .engine
            .write()
            .map_err(|_| EngineError::Unsupported("engine rwlock poisoned".into()))?;
        engine.execute(sql)
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
}

impl CannedResponse {
    fn single_text(col: &'static str, val: &'static str) -> Self {
        Self::Rows {
            columns: vec![ColumnSchema::new(col, DataType::Text, false)],
            rows: vec![Row::new(vec![Value::Text(val.to_string())])],
        }
    }
}

fn send_canned(stream: &mut TcpStream, c: &CannedResponse) -> std::io::Result<()> {
    match c {
        CannedResponse::Rows { columns, rows } => {
            send_row_description(stream, columns)?;
            for row in rows {
                send_data_row(stream, columns, row)?;
            }
            send_command_complete(stream, &format!("SELECT {}", rows.len()))?;
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

#[derive(Debug, Clone)]
struct PreparedStmt {
    sql: String,
    /// Number of `$N` placeholders we expect in the SQL. PG drivers
    /// declare param types in the Parse message; we ignore the
    /// declared types and accept whatever the Bind hands over,
    /// substituting via text-format conversion.
    placeholder_count: u16,
}

#[derive(Debug, Clone)]
struct Portal {
    /// SQL with parameter placeholders already substituted (text
    /// format only; binary params are rejected at Bind time).
    bound_sql: String,
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
    // Trailing u16 = param-type count, then that many u32 OIDs. We
    // ignore the declared types; placeholder_count is sourced from
    // the SQL by counting $N occurrences (cheap, robust).
    if cur + 2 > body.len() {
        return Err("Parse: missing parameter type count".into());
    }
    let _declared = u16::from_be_bytes([body[cur], body[cur + 1]]);
    let placeholder_count = count_placeholders(&sql);
    prepared.insert(
        name,
        PreparedStmt {
            sql,
            placeholder_count,
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
    // 0 = text, 1 = binary. We only support text for v4.7.
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
    // Param values count (u16), then for each: [i32 len][bytes...].
    // -1 = SQL NULL.
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
    let mut params: Vec<Option<String>> = Vec::with_capacity(param_count);
    for i in 0..param_count {
        if cur + 4 > body.len() {
            return Err("Bind: truncated parameter length".into());
        }
        let len = i32::from_be_bytes([body[cur], body[cur + 1], body[cur + 2], body[cur + 3]]);
        cur += 4;
        if len < 0 {
            params.push(None);
            continue;
        }
        let len = len as usize;
        if cur + len > body.len() {
            return Err("Bind: parameter value truncated".into());
        }
        // Format: 0 = text (default if formats empty), 1 = binary.
        // If only one format code provided, it applies to all params.
        let fmt = match formats.len() {
            0 => 0,
            1 => formats[0],
            _ => formats.get(i).copied().unwrap_or(0),
        };
        if fmt != 0 {
            return Err("Bind: binary parameter format not supported (v4.7 text-only)".into());
        }
        let s = std::str::from_utf8(&body[cur..cur + len])
            .map_err(|_| "Bind: text parameter not valid UTF-8".to_string())?
            .to_string();
        params.push(Some(s));
        cur += len;
    }
    // Trailing result-format-codes — we always return text, so ignore.
    let bound_sql = substitute_placeholders(&stmt.sql, &params);
    Ok((portal_name, Portal { bound_sql }))
}

/// Replace `$1`, `$2`, etc. in `sql` with the corresponding params.
/// String parameters are SQL-quoted (`'...'` with `''` escape).
/// NULL parameters render as the literal `NULL`. Numeric-looking
/// strings keep their text form — the engine's parser coerces them
/// per the column types.
fn substitute_placeholders(sql: &str, params: &[Option<String>]) -> String {
    let bytes = sql.as_bytes();
    let mut out = String::with_capacity(
        sql.len()
            + params
                .iter()
                .map(|p| p.as_ref().map_or(4, |s| s.len() + 2))
                .sum::<usize>(),
    );
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'$' && i + 1 < bytes.len() && bytes[i + 1].is_ascii_digit() {
            let mut j = i + 1;
            let mut n: usize = 0;
            while j < bytes.len() && bytes[j].is_ascii_digit() {
                n = n * 10 + (bytes[j] - b'0') as usize;
                j += 1;
            }
            if n >= 1 && n <= params.len() {
                match &params[n - 1] {
                    None => out.push_str("NULL"),
                    Some(v) => {
                        // Numeric-looking values render bare so the
                        // engine sees an `INT` literal rather than a
                        // `TEXT` literal it would then refuse to
                        // coerce into an INT column. Booleans get the
                        // same treatment.
                        if looks_numeric(v)
                            || matches!(v.as_str(), "true" | "false" | "TRUE" | "FALSE")
                        {
                            out.push_str(v);
                        } else {
                            // Quote + escape `'` as `''` per SQL.
                            out.push('\'');
                            for ch in v.chars() {
                                if ch == '\'' {
                                    out.push('\'');
                                }
                                out.push(ch);
                            }
                            out.push('\'');
                        }
                    }
                }
                i = j;
                continue;
            }
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out
}

/// True when `s` parses as a decimal integer or float — matching the
/// grammar `[+-]?[0-9]+(.[0-9]+)?([eE][+-]?[0-9]+)?`. Used to decide
/// whether to substitute a bind parameter as a bare literal vs a
/// quoted string. We deliberately keep this narrow to avoid letting
/// SQL fragments slip through unquoted.
fn looks_numeric(s: &str) -> bool {
    let s = s.trim();
    if s.is_empty() {
        return false;
    }
    // Reject anything with non-numeric punctuation that an integer
    // or float parser would reject.
    s.parse::<i64>().is_ok() || s.parse::<f64>().is_ok()
}

fn handle_execute(
    body: &[u8],
    portals: &std::collections::HashMap<String, Portal>,
    stream: &mut TcpStream,
    state: &Arc<ServerState>,
    role: Role,
    tx_state: &mut u8,
) -> Result<(), String> {
    let mut cur = 0;
    let portal_name = read_cstring(body, &mut cur)
        .ok_or("Execute: portal name not UTF-8")?
        .to_string();
    // Max-rows (i32, 0 = unlimited). We always return everything;
    // SELECT result sets in SPG are typically bounded, and respecting
    // a partial cursor would mean keeping the result Vec around
    // across Execute calls. Future work.
    if cur + 4 > body.len() {
        return Err("Execute: missing max-rows".into());
    }
    let portal = portals
        .get(&portal_name)
        .ok_or_else(|| format!("Execute: portal {portal_name:?} not found"))?;
    // Allow the canned (pg_catalog etc.) path to handle the bound SQL
    // too, so an ORM that always prepares `SELECT version()` still
    // gets the right reply.
    if let Some(canned) = canned_response(&portal.bound_sql, state) {
        send_canned(stream, &canned).map_err(|e| e.to_string())?;
        return Ok(());
    }
    let result = execute_with_role(state, &portal.bound_sql, role);
    match result {
        Ok(QueryResult::Rows { columns, rows }) => {
            send_row_description(stream, &columns).map_err(|e| e.to_string())?;
            let n = rows.len();
            for row in &rows {
                send_data_row(stream, &columns, row).map_err(|e| e.to_string())?;
            }
            send_command_complete(stream, &format!("SELECT {n}")).map_err(|e| e.to_string())?;
        }
        Ok(QueryResult::CommandOk { affected, .. }) => {
            let tag = command_tag(&portal.bound_sql, affected);
            send_command_complete(stream, &tag).map_err(|e| e.to_string())?;
            *tx_state = if state.engine.read().is_ok_and(|e| e.in_transaction()) {
                b'T'
            } else {
                b'I'
            };
        }
        Err(e) => return Err(e.to_string()),
    }
    Ok(())
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

fn send_msg(stream: &mut TcpStream, ty: u8, body: &[u8]) -> std::io::Result<()> {
    let len = u32::try_from(body.len() + 4)
        .map_err(|_| std::io::Error::other("PG message body too large"))?;
    let mut out = Vec::with_capacity(5 + body.len());
    out.push(ty);
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(body);
    stream.write_all(&out)
}

fn send_parameter_status(stream: &mut TcpStream, key: &str, value: &str) -> std::io::Result<()> {
    let mut body = Vec::with_capacity(key.len() + value.len() + 2);
    body.extend_from_slice(key.as_bytes());
    body.push(0);
    body.extend_from_slice(value.as_bytes());
    body.push(0);
    send_msg(stream, b'S', &body)
}

fn send_ready_for_query(stream: &mut TcpStream, state: u8) -> std::io::Result<()> {
    send_msg(stream, b'Z', &[state])
}

fn send_command_complete(stream: &mut TcpStream, tag: &str) -> std::io::Result<()> {
    let mut body = Vec::with_capacity(tag.len() + 1);
    body.extend_from_slice(tag.as_bytes());
    body.push(0);
    send_msg(stream, b'C', &body)
}

fn send_error(stream: &mut TcpStream, sqlstate: &str, msg: &str) -> std::io::Result<()> {
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

fn send_row_description(stream: &mut TcpStream, cols: &[ColumnSchema]) -> std::io::Result<()> {
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

fn send_data_row(stream: &mut TcpStream, cols: &[ColumnSchema], row: &Row) -> std::io::Result<()> {
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
        DataType::Text | DataType::Varchar(_) | DataType::Char(_) | DataType::Vector(_) => 25,
        DataType::Timestamp => 1114,
        DataType::Date => 1082,
        DataType::Interval => 1186,
        DataType::Numeric { .. } => 1700,
    }
}

const fn pg_type_len(ty: DataType) -> i16 {
    match ty {
        DataType::Bool => 1,
        DataType::SmallInt => 2,
        DataType::Int | DataType::Date => 4,
        DataType::BigInt | DataType::Float | DataType::Timestamp => 8,
        DataType::Interval => 16,
        _ => -1, // varlena
    }
}

fn value_to_pg_text(v: &Value, _ty: Option<DataType>) -> Option<String> {
    Some(match v {
        Value::Null => return None,
        Value::Bool(b) => if *b { "t" } else { "f" }.to_string(),
        Value::SmallInt(n) => n.to_string(),
        Value::Int(n) => n.to_string(),
        Value::BigInt(n) => n.to_string(),
        Value::Float(f) => format!("{f}"),
        Value::Text(s) => s.clone(),
        Value::Timestamp(micros) => format_timestamp(*micros),
        Value::Date(days) => format_date(*days),
        Value::Interval { months, micros } => format!("P{months}M{micros}U"),
        Value::Numeric { scaled, scale } => format_numeric(*scaled, *scale),
        Value::Vector(v) => {
            let parts: Vec<String> = v.iter().map(std::string::ToString::to_string).collect();
            format!("[{}]", parts.join(", "))
        }
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
