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
                if let Some(canned) = canned_response(&sql) {
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
            // ParseDescribeBindExecuteSync (extended query) — we don't
            // support, but psql's `\d` works around it via simple
            // query. Just send an empty ReadyForQuery and hope.
            b'P' | b'B' | b'D' | b'E' | b'C' | b'H' | b'S' | b'F' => {
                send_error(
                    &mut stream,
                    "0A000",
                    "extended query protocol not supported",
                )?;
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

/// Canned answers for client startup probes — saves us implementing
/// PG catalog tables just to make `psql` not bail on connect.
fn canned_response(sql: &str) -> Option<CannedResponse> {
    let lower = sql.trim().to_ascii_lowercase();
    if lower.starts_with("select version()") || lower == "select version()" {
        return Some(CannedResponse::SingleText("version", "spg 4.3"));
    }
    if lower.starts_with("show transaction_isolation")
        || lower.starts_with("show transaction isolation level")
    {
        return Some(CannedResponse::SingleText(
            "transaction_isolation",
            "read committed",
        ));
    }
    if lower.starts_with("show search_path") || lower == "show search_path" {
        return Some(CannedResponse::SingleText(
            "search_path",
            "\"$user\", public",
        ));
    }
    if lower.starts_with("show standard_conforming_strings") {
        return Some(CannedResponse::SingleText(
            "standard_conforming_strings",
            "on",
        ));
    }
    None
}

enum CannedResponse {
    SingleText(&'static str, &'static str),
}

fn send_canned(stream: &mut TcpStream, c: &CannedResponse) -> std::io::Result<()> {
    match c {
        CannedResponse::SingleText(col, val) => {
            let cols = vec![ColumnSchema::new(*col, DataType::Text, false)];
            send_row_description(stream, &cols)?;
            let row = Row::new(vec![Value::Text((*val).to_string())]);
            send_data_row(stream, &cols, &row)?;
            send_command_complete(stream, "SELECT 1")?;
        }
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
