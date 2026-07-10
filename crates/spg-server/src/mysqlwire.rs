// MySQL wire protocol is a hand-rolled binary protocol. Casts
// between integer widths (u24 lengths, u16 cap-flag halves) and
// verbose match arms are inherent to the format. Allowlist
// scoped to this file, mirroring pgwire.rs.
#![allow(
    clippy::cast_lossless,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::doc_markdown,
    clippy::similar_names,
    clippy::too_many_lines,
    clippy::uninlined_format_args,
    clippy::unreadable_literal
)]

//! v7.17.0 Phase 3.P0-70 — MySQL wire-protocol compatibility shim
//! (foundation: HandshakeV10 + HandshakeResponse41 packet framing).
//!
//! Opt-in: set `SPG_MYSQLWIRE_ADDR=127.0.0.1:3307` (or any
//! host:port) and the server starts a third TCP listener (after
//! native + pgwire) that talks the MySQL `Protocol::HandshakeV10`
//! handshake. Goal of the full Segment G is "mysql CLI / DBeaver
//! / MySQL Workbench / sqlx-mysql can connect against the same
//! Engine instance".
//!
//! P0-70 lands the framing + handshake exchange ONLY. The
//! follow-on P0s wire auth verification (P0-71/P0-72), command
//! handling (P0-73/P0-74/P0-75), binary result encoding (P0-76),
//! and SSL upgrade (P0-77).
//!
//! Packet framing is fixed (every MySQL packet on the wire):
//!   bytes 0..3 — payload length, little-endian u24
//!   byte  3    — sequence id (starts at 0 from the server's
//!                first greeting, then client and server
//!                alternate seqno increments per RTT step)
//!   bytes 4..  — payload bytes
//!
//! Reference: <https://dev.mysql.com/doc/dev/mysql-server/latest/page_protocol_basic_packets.html>

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::thread;

use spg_engine::QueryResult;
use spg_storage::{ColumnSchema, DataType, Value};

use crate::ServerState;

/// v7.17.0 Phase 3.P0-77 — trait object surface that bridges
/// plain TCP and the rustls-wrapped TLS stream. Every command-
/// loop helper takes `&mut dyn ConnIo` so we don't have to
/// monomorphise the entire surface twice. Blanket impl below
/// covers both `std::net::TcpStream` and
/// `rustls::Stream<'_, ServerConnection, TcpStream>`.
pub(crate) trait ReadWrite: Read + Write {}
impl<T: Read + Write + ?Sized> ReadWrite for T {}

// ---- capability flags ---------------------------------------

/// CLIENT_LONG_PASSWORD — historical (use ≥ 4.1 password).
pub(crate) const CLIENT_LONG_PASSWORD: u32 = 0x0000_0001;
/// CLIENT_FOUND_ROWS — affected-rows reports matched rather than
/// changed.
pub(crate) const CLIENT_FOUND_ROWS: u32 = 0x0000_0002;
/// CLIENT_LONG_FLAG — full column flag mask.
pub(crate) const CLIENT_LONG_FLAG: u32 = 0x0000_0004;
/// CLIENT_CONNECT_WITH_DB — schema name follows in
/// HandshakeResponse.
pub(crate) const CLIENT_CONNECT_WITH_DB: u32 = 0x0000_0008;
/// CLIENT_PROTOCOL_41 — modern protocol shape (we always
/// advertise this).
pub(crate) const CLIENT_PROTOCOL_41: u32 = 0x0000_0200;
/// CLIENT_TRANSACTIONS — server reports transaction status flags.
pub(crate) const CLIENT_TRANSACTIONS: u32 = 0x0000_2000;
/// CLIENT_SECURE_CONNECTION — uses the 4.1+ auth response shape.
pub(crate) const CLIENT_SECURE_CONNECTION: u32 = 0x0000_8000;
/// CLIENT_PLUGIN_AUTH — server names an auth plugin in
/// HandshakeV10 and may issue AuthSwitchRequest mid-handshake.
pub(crate) const CLIENT_PLUGIN_AUTH: u32 = 0x0008_0000;
/// CLIENT_CONNECT_ATTRS — k=v attrs follow auth response.
pub(crate) const CLIENT_CONNECT_ATTRS: u32 = 0x0010_0000;
/// CLIENT_PLUGIN_AUTH_LENENC_CLIENT_DATA — auth response length
/// is length-encoded, not 1-byte.
pub(crate) const CLIENT_PLUGIN_AUTH_LENENC_CLIENT_DATA: u32 = 0x0020_0000;
/// CLIENT_DEPRECATE_EOF — replace EOF packets with OK packets.
pub(crate) const CLIENT_DEPRECATE_EOF: u32 = 0x0100_0000;
/// v7.17.0 Phase 3.P0-77 — CLIENT_SSL. Advertised in the
/// greeting so any MySQL client that supports the
/// `Protocol::SSLRequest` mid-handshake upgrade can opt in.
pub(crate) const CLIENT_SSL: u32 = 0x0000_0800;

/// Server-advertised capability mask for P0-70. Each follow-on
/// P0 ORs additional bits in as it implements the feature
/// (SSL, multi-result, etc.).
pub(crate) const SERVER_CAPABILITIES: u32 = CLIENT_LONG_PASSWORD
    | CLIENT_FOUND_ROWS
    | CLIENT_LONG_FLAG
    | CLIENT_CONNECT_WITH_DB
    | CLIENT_PROTOCOL_41
    | CLIENT_TRANSACTIONS
    | CLIENT_SECURE_CONNECTION
    | CLIENT_PLUGIN_AUTH
    | CLIENT_DEPRECATE_EOF
    | CLIENT_SSL;

/// utf8mb4 collation id — what MySQL 8.0+ servers advertise by
/// default.
const CHARSET_UTF8MB4: u8 = 0xff;

/// SERVER_STATUS_AUTOCOMMIT (we always run autocommit until
/// BEGIN is wired in a later P0).
const SERVER_STATUS_AUTOCOMMIT: u16 = 0x0002;

/// Auth plugin name advertised in the initial HandshakeV10. We
/// pick `mysql_native_password` since it's the simplest and
/// every client speaks it. v7.17.0 Phase 3.P0-72 also accepts
/// `caching_sha2_password` in the client's HandshakeResponse —
/// see `verify_native_password` / `verify_caching_sha2`.
pub(crate) const AUTH_PLUGIN_NATIVE: &str = "mysql_native_password";
/// v7.17.0 Phase 3.P0-72 — MySQL 8.0+ default plugin name.
pub(crate) const AUTH_PLUGIN_CACHING_SHA2: &str = "caching_sha2_password";

// ---- listener bootstrap -------------------------------------

/// Spawn the MySQL-wire listener thread. Returns once the
/// listener is bound (so the parent can log "listening on …").
/// Each accepted connection runs its own thread that owns the
/// `Conn` state — same shape as `pgwire::spawn_listener`.
///
/// # Errors
/// Surfaces bind / address errors from `TcpListener::bind`.
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
                    eprintln!("spg-server: mysql-wire conn error: {e}");
                }
            });
        }
    });
    Ok(local)
}

fn handle_conn(mut stream: TcpStream, state: &Arc<ServerState>) -> std::io::Result<()> {
    let _ = stream.set_nodelay(true);

    // ---- HandshakeV10 (server → client) ----------------------
    //
    // Connection id is just the local port to keep it
    // deterministic per-connection without an extra atomic.
    // The full Segment G binds it to ServerState.active_connections
    // once command handling needs the id for KILL.
    let conn_id = stream
        .local_addr()
        .map(|a| u32::from(a.port()))
        .unwrap_or(1);
    // 20-byte auth-plugin-data scramble. Deterministic random
    // from the connection id + clock so retries reproduce the
    // same bytes during e2e testing without a separate seed
    // channel. Production strength is added in P0-71 (where
    // mysql_native_password verification proves the scramble
    // was actually random).
    let scramble = generate_scramble(conn_id);

    let mut greeting = HandshakeV10Greeting {
        protocol_version: 10,
        server_version: server_version_string(),
        connection_id: conn_id,
        scramble: scramble.clone(),
        capability_flags: SERVER_CAPABILITIES,
        character_set: CHARSET_UTF8MB4,
        status_flags: SERVER_STATUS_AUTOCOMMIT,
        auth_plugin_name: AUTH_PLUGIN_NATIVE.to_string(),
    };
    // seqno starts at 0 for the server's first greeting.
    write_packet(&mut stream, 0, &encode_handshake_v10(&greeting))?;

    // ---- SSLRequest / HandshakeResponse41 (client → server) ----
    //
    // v7.17.0 Phase 3.P0-77 — the first client packet can be
    // either a 32-byte `Protocol::SSLRequest` (cap flags + max
    // packet + charset + 23-byte filler, NO username) that
    // triggers a mid-handshake TLS upgrade, or a full
    // HandshakeResponse41. Detect by length + CLIENT_SSL bit.
    let (seqno_in, payload) = read_packet(&mut stream)?;
    if seqno_in != 1 {
        return write_packet(
            &mut stream,
            seqno_in.wrapping_add(1),
            &encode_err_packet(
                1043,
                "08S01",
                &format!("bad handshake: expected client seqno 1, got {seqno_in}"),
            ),
        );
    }
    if looks_like_ssl_request(&payload) {
        // ---- TLS upgrade ----
        let mut tls_conn = match build_server_connection() {
            Ok(c) => c,
            Err(e) => {
                return write_packet(
                    &mut stream,
                    seqno_in.wrapping_add(1),
                    &encode_err_packet(
                        2026,
                        "08000",
                        &format!("SSL: server config init failed: {e}"),
                    ),
                );
            }
        };
        // rustls::Stream owns the bridge between the connection
        // state and the TCP socket. complete_io drives both
        // halves of the handshake bytes through the socket. Then
        // we read the post-handshake HandshakeResponse41 packet
        // off the encrypted stream.
        let mut tls_stream = rustls::Stream::new(&mut tls_conn, &mut stream);
        let (seqno_in, payload) = read_packet(&mut tls_stream)?;
        let parsed = match parse_handshake_response_41(&payload) {
            Ok(r) => r,
            Err(msg) => {
                return write_packet(
                    &mut tls_stream,
                    seqno_in.wrapping_add(1),
                    &encode_err_packet(1043, "08S01", &msg),
                );
            }
        };
        let scramble = std::mem::take(&mut greeting.scramble);
        return complete_auth_and_command(&mut tls_stream, state, &parsed, &scramble, seqno_in);
    }
    let parsed = match parse_handshake_response_41(&payload) {
        Ok(r) => r,
        Err(msg) => {
            return write_packet(
                &mut stream,
                seqno_in.wrapping_add(1),
                &encode_err_packet(1043, "08S01", &msg),
            );
        }
    };

    // v7.17.0 Phase 3.P0-71 / P0-72 — verify the auth response.
    // Open mode (engine has no users) accepts any client and
    // sends OK; users-present mode dispatches on the plugin
    // name the client claimed:
    //   * `mysql_native_password` → SHA1(SHA1(pw)) proof
    //   * `caching_sha2_password` → SHA256(SHA256(pw)) fast path,
    //     followed by an Auth More Data (0x01 + 0x03 fast auth
    //     success) packet then OK. RSA full-auth is a v7.18
    //     carve-out — fast-path failures surface as Access
    //     Denied here.
    let scramble = std::mem::take(&mut greeting.scramble);
    complete_auth_and_command(&mut stream, state, &parsed, &scramble, seqno_in)
}

/// v7.17.0 Phase 3.P0-77 — auth verification + command loop
/// generic over the underlying stream. Plain-TCP and TLS paths
/// converge here so the auth code only exists once.
fn complete_auth_and_command(
    stream: &mut dyn ReadWrite,
    state: &Arc<ServerState>,
    parsed: &HandshakeResponse41,
    scramble: &[u8],
    seqno_in: u8,
) -> std::io::Result<()> {
    let auth_outcome = verify_handshake_response(state, parsed, scramble);
    let reply_seqno = seqno_in.wrapping_add(1);
    match auth_outcome {
        AuthOutcome::Ok => {
            write_packet(stream, reply_seqno, &encode_ok_packet())?;
        }
        AuthOutcome::CachingSha2FastAuthOk => {
            write_packet(stream, reply_seqno, &[0x01, 0x03])?;
            write_packet(stream, reply_seqno.wrapping_add(1), &encode_ok_packet())?;
        }
        AuthOutcome::AccessDenied(msg) => {
            return write_packet(stream, reply_seqno, &encode_err_packet(1045, "28000", &msg));
        }
        AuthOutcome::PluginMismatch(plugin) => {
            return write_packet(
                stream,
                reply_seqno,
                &encode_err_packet(
                    1251,
                    "08004",
                    &format!(
                        "auth plugin {plugin:?} not yet supported — Segment G covers mysql_native_password and caching_sha2_password (fast path)",
                    ),
                ),
            );
        }
    }
    command_loop(stream, state)
}

/// v7.17.0 Phase 3.P0-77 — detect the `Protocol::SSLRequest`
/// packet shape: exactly 32 bytes (cap flags 4 + max packet 4 +
/// charset 1 + 23-byte reserved filler) AND the CLIENT_SSL bit
/// is set in the cap flags. HandshakeResponse41 always has more
/// than 32 bytes (at least a username + auth bytes), so the
/// disambiguation is unambiguous.
fn looks_like_ssl_request(payload: &[u8]) -> bool {
    if payload.len() != 32 {
        return false;
    }
    let caps = u32::from_le_bytes(payload[..4].try_into().unwrap_or([0; 4]));
    caps & CLIENT_SSL != 0
}

/// v7.17.0 Phase 3.P0-77 — build a rustls server connection
/// against the process-global self-signed cert. The cert is
/// minted lazily on first connection so the listener startup
/// cost stays zero when nothing's connected yet. Operator
/// deployments that need a real cert hook `SPG_MYSQLWIRE_TLS_*`
/// env vars in a follow-on commit.
pub(crate) fn build_server_connection() -> Result<rustls::ServerConnection, String> {
    let cfg = tls_server_config()?;
    rustls::ServerConnection::new(cfg).map_err(|e| format!("rustls accept: {e}"))
}

fn tls_server_config() -> Result<Arc<rustls::ServerConfig>, String> {
    static CFG: std::sync::OnceLock<Result<Arc<rustls::ServerConfig>, String>> =
        std::sync::OnceLock::new();
    CFG.get_or_init(|| {
        // Install the ring crypto provider if no provider has
        // been registered yet. rustls 0.23 demands an explicit
        // pick; ring is what we depend on.
        let _ = rustls::crypto::ring::default_provider().install_default();

        let cert = rcgen::generate_simple_self_signed(vec!["localhost".to_string()])
            .map_err(|e| format!("rcgen: {e}"))?;
        let cert_der_bytes = cert.cert.der().to_vec();
        let key_der_bytes = cert.key_pair.serialize_der();
        let cert_der = rustls::pki_types::CertificateDer::from(cert_der_bytes);
        let key_der = rustls::pki_types::PrivateKeyDer::Pkcs8(
            rustls::pki_types::PrivatePkcs8KeyDer::from(key_der_bytes),
        );
        let cfg = rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(vec![cert_der], key_der)
            .map_err(|e| format!("rustls cert: {e}"))?;
        Ok(Arc::new(cfg))
    })
    .clone()
}

/// v7.17.0 Phase 3.P0-74 — per-session prepared-statement
/// state. `stmt_id` is server-assigned, 1-based; we never
/// recycle slots within one session so a stale id from a
/// long-running connector never lands on a fresh statement.
#[derive(Default)]
struct PreparedState {
    next_id: u32,
    by_id: std::collections::HashMap<u32, PreparedEntry>,
}

struct PreparedEntry {
    sql: String,
    /// Output column schemas captured at PREPARE so EXECUTE
    /// returns the same column shape without re-describing.
    columns: Vec<ColumnSchema>,
    /// Number of `$N` / `?` placeholders the SQL declares.
    param_count: u16,
}

fn command_loop(
    stream: &mut (dyn ReadWrite + '_),
    state: &Arc<ServerState>,
) -> std::io::Result<()> {
    let mut prepared = PreparedState::default();
    loop {
        let (seqno_in, payload) = match read_packet(stream) {
            Ok(t) => t,
            // Client closed the connection — normal exit.
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(()),
            Err(e) => return Err(e),
        };
        if payload.is_empty() {
            return write_packet(
                stream,
                seqno_in.wrapping_add(1),
                &encode_err_packet(1064, "42000", "empty command packet"),
            );
        }
        let cmd = payload[0];
        let reply_seqno = seqno_in.wrapping_add(1);
        match cmd {
            CMD_QUIT => return Ok(()),
            CMD_QUERY => {
                let sql = std::str::from_utf8(&payload[1..]).unwrap_or("").to_string();
                handle_com_query(stream, state, &sql, reply_seqno)?;
            }
            CMD_PING => {
                // Single OK packet, no body — what mysqladmin
                // ping expects.
                write_packet(stream, reply_seqno, &encode_ok_packet())?;
            }
            CMD_INIT_DB => {
                // SPG runs in single-database mode. mysql clients
                // issue COM_INIT_DB after `USE <db>`; we accept
                // any non-empty name with OK so the client can
                // proceed. Empty name → 1049 unknown database.
                let db_name = std::str::from_utf8(&payload[1..]).unwrap_or("").trim();
                if db_name.is_empty() {
                    write_packet(
                        stream,
                        reply_seqno,
                        &encode_err_packet(1049, "42000", "Unknown database (empty name)"),
                    )?;
                } else {
                    write_packet(stream, reply_seqno, &encode_ok_packet())?;
                }
            }
            CMD_FIELD_LIST => {
                handle_com_field_list(stream, state, &payload[1..], reply_seqno)?;
            }
            CMD_STMT_PREPARE => {
                let sql = std::str::from_utf8(&payload[1..]).unwrap_or("").to_string();
                handle_com_stmt_prepare(stream, state, &mut prepared, &sql, reply_seqno)?;
            }
            CMD_STMT_EXECUTE => {
                handle_com_stmt_execute(stream, state, &mut prepared, &payload[1..], reply_seqno)?;
            }
            CMD_STMT_CLOSE => {
                // COM_STMT_CLOSE has no response — server just
                // forgets the statement. Stale ids are ignored.
                if payload.len() >= 5 {
                    let id = u32::from_le_bytes(payload[1..5].try_into().unwrap());
                    prepared.by_id.remove(&id);
                }
            }
            CMD_STMT_RESET => {
                // COM_STMT_RESET: clear long-data buffers. SPG
                // doesn't support COM_STMT_SEND_LONG_DATA yet, so
                // this is effectively a no-op — but we still
                // reply OK so the client doesn't error.
                write_packet(stream, reply_seqno, &encode_ok_packet())?;
            }
            _ => {
                // Unknown command. P0-74 / P0-75 add binary-protocol
                // commands (STMT_PREPARE / EXECUTE / RESET / CLOSE,
                // PING / INIT_DB / FIELD_LIST). Until then surface
                // a structured error.
                write_packet(
                    stream,
                    reply_seqno,
                    &encode_err_packet(
                        1047,
                        "08S01",
                        &format!(
                            "unknown MySQL command 0x{cmd:02x} (Segment G follow-ons land in P0-74/75)"
                        ),
                    ),
                )?;
            }
        }
    }
}

// ---- COM_QUERY handler --------------------------------------

pub(crate) const CMD_QUIT: u8 = 0x01;
/// v7.17.0 Phase 3.P0-75 — `USE <db>` from the mysql CLI. SPG
/// is single-database; we accept the command for compatibility
/// but the payload (target database name) doesn't change state.
pub(crate) const CMD_INIT_DB: u8 = 0x02;
pub(crate) const CMD_QUERY: u8 = 0x03;
/// v7.17.0 Phase 3.P0-75 — `mysqldump --opt` issues this for
/// every table to learn the column shape ahead of `SELECT * …`.
pub(crate) const CMD_FIELD_LIST: u8 = 0x04;
/// v7.17.0 Phase 3.P0-75 — `mysqladmin ping` / connector keepalive.
pub(crate) const CMD_PING: u8 = 0x0e;
/// v7.17.0 Phase 3.P0-74 — binary prepared protocol.
pub(crate) const CMD_STMT_PREPARE: u8 = 0x16;
pub(crate) const CMD_STMT_EXECUTE: u8 = 0x17;
pub(crate) const CMD_STMT_CLOSE: u8 = 0x19;
pub(crate) const CMD_STMT_RESET: u8 = 0x1a;

fn handle_com_query(
    stream: &mut (dyn ReadWrite + '_),
    state: &Arc<ServerState>,
    sql: &str,
    start_seqno: u8,
) -> std::io::Result<()> {
    // Run the SQL through the engine. Mirrors what pgwire's
    // 'Q' (simple query) path does — single-statement form, no
    // transaction state changes here (autocommit-only for v7.17
    // mysql-wire; BEGIN/COMMIT through pgwire still works).
    let outcome = {
        let Ok(mut engine) = state.engine.write() else {
            return write_packet(
                stream,
                start_seqno,
                &encode_err_packet(1815, "HY000", "engine lock poisoned"),
            );
        };
        engine.execute(sql)
    };
    // v7.33 (A1) — persist the write (WAL/snapshot + audit) before
    // acking. The mysql-wire path was non-durable like pgwire pre-7.33:
    // a COM_QUERY write was lost on crash. A durability failure turns
    // the write into an error, never a silent OK.
    let outcome = match crate::pgwire::persist_wire_write(state, sql, &outcome) {
        Ok(()) => outcome,
        Err(e) => Err(spg_engine::EngineError::Unsupported(format!(
            "durability append failed: {e}"
        ))),
    };
    match outcome {
        Err(e) => {
            // Map engine errors to MySQL errno 1064 (parse / unsupported)
            // unless the error is a typed mismatch (1146 missing table)
            // — but the engine doesn't surface that today, so just
            // surface 1064 with the message.
            let msg = format!("{e}");
            write_packet(stream, start_seqno, &encode_err_packet(1064, "42000", &msg))?;
        }
        Ok(QueryResult::CommandOk { affected, .. }) => {
            write_packet(
                stream,
                start_seqno,
                &encode_ok_with_affected(affected as u64),
            )?;
        }
        Ok(QueryResult::Rows { columns, rows }) => {
            encode_text_result_set(stream, &columns, &rows, start_seqno)?;
        }
        // `QueryResult` is `#[non_exhaustive]` — future variants
        // surface as a structured error here until the wire shim
        // learns about them.
        Ok(_) => {
            write_packet(
                stream,
                start_seqno,
                &encode_err_packet(
                    1815,
                    "HY000",
                    "engine returned a QueryResult variant the MySQL-wire shim doesn't yet encode",
                ),
            )?;
        }
    }
    Ok(())
}

// ---- COM_STMT_PREPARE / EXECUTE ----------------------------

fn handle_com_stmt_prepare(
    stream: &mut (dyn ReadWrite + '_),
    state: &Arc<ServerState>,
    prepared: &mut PreparedState,
    sql: &str,
    start_seqno: u8,
) -> std::io::Result<()> {
    let (param_count, columns) = {
        let Ok(mut engine) = state.engine.write() else {
            return write_packet(
                stream,
                start_seqno,
                &encode_err_packet(1815, "HY000", "engine lock poisoned"),
            );
        };
        match engine.prepare(sql) {
            Ok(stmt) => {
                let (param_oids, cols) = engine.describe_prepared(&stmt);
                (param_oids.len() as u16, cols)
            }
            Err(e) => {
                let msg = format!("{e:?}");
                return write_packet(stream, start_seqno, &encode_err_packet(1064, "42000", &msg));
            }
        }
    };
    let id = prepared.next_id.wrapping_add(1);
    prepared.next_id = id;
    let num_columns = columns.len() as u16;
    let entry = PreparedEntry {
        sql: sql.to_string(),
        columns: columns.clone(),
        param_count,
    };
    prepared.by_id.insert(id, entry);

    // ---- COM_STMT_PREPARE_OK packet ----
    // status(1) + stmt_id(4) + num_columns(2) + num_params(2)
    // + reserved(1) + warning_count(2)
    let mut payload = Vec::with_capacity(12);
    payload.push(0x00);
    payload.extend_from_slice(&id.to_le_bytes());
    payload.extend_from_slice(&num_columns.to_le_bytes());
    payload.extend_from_slice(&param_count.to_le_bytes());
    payload.push(0x00); // reserved
    payload.extend_from_slice(&0u16.to_le_bytes()); // warnings
    let mut seq = start_seqno;
    write_packet(stream, seq, &payload)?;
    seq = seq.wrapping_add(1);
    // ---- parameter definitions ----
    if param_count > 0 {
        for _ in 0..param_count {
            // SPG describe_prepared doesn't surface per-parameter
            // type info today — we hand back a generic
            // VAR_STRING column_def so the bind path remains
            // wire-correct. EXECUTE then dispatches on the
            // type byte the client sent.
            let placeholder = ColumnSchema {
                name: "?".to_string(),
                ty: DataType::Text,
                nullable: true,
                auto_increment: false,
                default: None,
                runtime_default: None,
                user_enum_type: None,
                user_domain_type: None,
                on_update_runtime: None,
                collation: spg_storage::Collation::Binary,
                is_unsigned: false,
                inline_enum_variants: None,
                inline_set_variants: None,
                generated_stored_expr: None,
                identity_always: false,
                default_text: None,
            };
            let buf = encode_column_def_41(&placeholder);
            write_packet(stream, seq, &buf)?;
            seq = seq.wrapping_add(1);
        }
        // Intermediate marker (CLIENT_DEPRECATE_EOF → OK).
        write_packet(stream, seq, &encode_ok_packet())?;
        seq = seq.wrapping_add(1);
    }
    // ---- result column definitions ----
    if num_columns > 0 {
        for c in &columns {
            let buf = encode_column_def_41(c);
            write_packet(stream, seq, &buf)?;
            seq = seq.wrapping_add(1);
        }
        write_packet(stream, seq, &encode_ok_packet())?;
    }
    Ok(())
}

fn handle_com_stmt_execute(
    stream: &mut (dyn ReadWrite + '_),
    state: &Arc<ServerState>,
    prepared: &mut PreparedState,
    payload: &[u8],
    start_seqno: u8,
) -> std::io::Result<()> {
    if payload.len() < 9 {
        return write_packet(
            stream,
            start_seqno,
            &encode_err_packet(1064, "42000", "COM_STMT_EXECUTE payload truncated"),
        );
    }
    let stmt_id = u32::from_le_bytes(payload[..4].try_into().unwrap());
    let _flags = payload[4];
    let _iteration = u32::from_le_bytes(payload[5..9].try_into().unwrap());

    let entry = match prepared.by_id.get(&stmt_id) {
        Some(e) => e.clone_for_exec(),
        None => {
            return write_packet(
                stream,
                start_seqno,
                &encode_err_packet(
                    1243,
                    "HY000",
                    &format!("Unknown prepared statement handler ({stmt_id})"),
                ),
            );
        }
    };

    // Parse parameters per the binary protocol.
    let params = match parse_execute_params(&entry, &payload[9..]) {
        Ok(p) => p,
        Err(msg) => {
            return write_packet(stream, start_seqno, &encode_err_packet(1064, "42000", &msg));
        }
    };

    // Bind + execute via engine.
    let (outcome, render) = {
        let Ok(mut engine) = state.engine.write() else {
            return write_packet(
                stream,
                start_seqno,
                &encode_err_packet(1815, "HY000", "engine lock poisoned"),
            );
        };
        let stmt = match engine.prepare(&entry.sql) {
            Ok(s) => s,
            Err(e) => {
                let msg = format!("{e:?}");
                return write_packet(stream, start_seqno, &encode_err_packet(1064, "42000", &msg));
            }
        };
        // v7.33 (A1) — render the bind-final SQL for the WAL before
        // execute_prepared consumes the statement (writes only; a
        // substitute failure leaves render None and execute reports it).
        let render = if matches!(&stmt, spg_sql::ast::Statement::Select(_)) {
            None
        } else {
            let mut bind = stmt.clone();
            spg_engine::substitute_placeholders(&mut bind, &params)
                .ok()
                .map(|()| bind.to_string())
        };
        (engine.execute_prepared(stmt, &params), render)
    };
    // v7.33 (A1) — persist the prepared write before acking (was
    // non-durable pre-7.33, lost on crash).
    let outcome = match render {
        Some(render) => match crate::pgwire::persist_wire_write(state, &render, &outcome) {
            Ok(()) => outcome,
            Err(e) => Err(spg_engine::EngineError::Unsupported(format!(
                "durability append failed: {e}"
            ))),
        },
        None => outcome,
    };
    match outcome {
        Err(e) => {
            let msg = format!("{e}");
            write_packet(stream, start_seqno, &encode_err_packet(1064, "42000", &msg))?;
        }
        Ok(QueryResult::CommandOk { affected, .. }) => {
            write_packet(
                stream,
                start_seqno,
                &encode_ok_with_affected(affected as u64),
            )?;
        }
        Ok(QueryResult::Rows { columns, rows }) => {
            // v7.17.0 Phase 3.P0-76 — true binary result rows.
            encode_binary_result_set(stream, &columns, &rows, start_seqno)?;
        }
        Ok(_) => {
            write_packet(
                stream,
                start_seqno,
                &encode_err_packet(
                    1815,
                    "HY000",
                    "engine returned a QueryResult variant the binary EXECUTE path doesn't yet encode",
                ),
            )?;
        }
    }
    Ok(())
}

// ---- binary result set (P0-76) ------------------------------

fn encode_binary_result_set(
    stream: &mut (dyn ReadWrite + '_),
    columns: &[ColumnSchema],
    rows: &[spg_storage::Row],
    start_seqno: u8,
) -> std::io::Result<()> {
    // 1) column_count packet.
    let mut payload = Vec::new();
    encode_lenenc_int(&mut payload, columns.len() as u64);
    let mut seq = start_seqno;
    write_packet(stream, seq, &payload)?;
    seq = seq.wrapping_add(1);
    // 2) column_def_41 packets (same shape as text protocol).
    for c in columns {
        let buf = encode_column_def_41(c);
        write_packet(stream, seq, &buf)?;
        seq = seq.wrapping_add(1);
    }
    // 3) binary row packets.
    for row in rows {
        let buf = encode_binary_row(&row.values, columns);
        write_packet(stream, seq, &buf)?;
        seq = seq.wrapping_add(1);
    }
    // 4) trailing OK.
    write_packet(stream, seq, &encode_ok_packet())?;
    Ok(())
}

fn encode_binary_row(values: &[Value], columns: &[ColumnSchema]) -> Vec<u8> {
    let n = values.len();
    let bitmap_len = (n + 7 + 2) / 8;
    let mut out = Vec::with_capacity(1 + bitmap_len + values.len() * 8);
    out.push(0x00); // packet header
    // Reserve space for the NULL bitmap; we'll fill in shortly.
    let bitmap_start = out.len();
    out.extend(std::iter::repeat_n(0u8, bitmap_len));
    for (i, v) in values.iter().enumerate() {
        if matches!(v, Value::Null) {
            let bit_idx = i + 2; // first 2 bits reserved
            out[bitmap_start + bit_idx / 8] |= 1 << (bit_idx % 8);
            continue;
        }
        let ty = columns.get(i).map(|c| c.ty);
        encode_binary_value(&mut out, v, ty);
    }
    out
}

fn encode_binary_value(out: &mut Vec<u8>, v: &Value, declared: Option<DataType>) {
    match v {
        Value::Null => {} // handled by NULL bitmap caller-side
        Value::Bool(b) => out.push(u8::from(*b)),
        Value::SmallInt(n) => out.extend_from_slice(&n.to_le_bytes()),
        Value::Int(n) => out.extend_from_slice(&n.to_le_bytes()),
        Value::BigInt(n) => out.extend_from_slice(&n.to_le_bytes()),
        Value::Float(f) => out.extend_from_slice(&f.to_le_bytes()),
        Value::Text(s) | Value::Json(s) => {
            encode_lenenc_string(out, s.as_bytes());
        }
        Value::Date(days) => encode_binary_date(out, *days),
        Value::Timestamp(us) => {
            let is_tz = matches!(declared, Some(DataType::Timestamptz));
            encode_binary_datetime(out, *us, is_tz);
        }
        Value::Bytes(b) => encode_lenenc_string(out, b),
        // Everything else (Numeric, Vector, TsVector, Range,
        // Hstore, Money, UUID, Interval, Inet, MacAddr, etc.)
        // serialises as the engine's canonical text — same
        // shape clients see through the text-protocol path.
        other => {
            let text = value_to_mysql_text(other);
            encode_lenenc_string(out, text.as_bytes());
        }
    }
}

fn encode_binary_date(out: &mut Vec<u8>, days_since_epoch: i32) {
    // 1 byte length + (4 bytes: year u16, month u8, day u8)
    let (year, month, day) = ymd_from_days_since_epoch(days_since_epoch);
    out.push(4);
    out.extend_from_slice(&year.to_le_bytes());
    out.push(month);
    out.push(day);
}

fn encode_binary_datetime(out: &mut Vec<u8>, micros_since_epoch: i64, _is_tz: bool) {
    let days = micros_since_epoch.div_euclid(86_400_000_000) as i32;
    let intra_day_us = micros_since_epoch.rem_euclid(86_400_000_000) as u64;
    let (year, month, day) = ymd_from_days_since_epoch(days);
    let hour = (intra_day_us / 3_600_000_000) as u8;
    let rem = intra_day_us % 3_600_000_000;
    let minute = (rem / 60_000_000) as u8;
    let rem = rem % 60_000_000;
    let second = (rem / 1_000_000) as u8;
    let us = rem % 1_000_000;
    if us == 0 && hour == 0 && minute == 0 && second == 0 {
        // Date-only DATETIME — 4-byte form.
        out.push(4);
        out.extend_from_slice(&year.to_le_bytes());
        out.push(month);
        out.push(day);
        return;
    }
    if us == 0 {
        out.push(7);
        out.extend_from_slice(&year.to_le_bytes());
        out.push(month);
        out.push(day);
        out.push(hour);
        out.push(minute);
        out.push(second);
        return;
    }
    out.push(11);
    out.extend_from_slice(&year.to_le_bytes());
    out.push(month);
    out.push(day);
    out.push(hour);
    out.push(minute);
    out.push(second);
    out.extend_from_slice(&(us as u32).to_le_bytes());
}

/// Civil-calendar split — same algorithm SPG's engine uses for
/// DATE rendering. Returns (year, month, day) where year fits
/// u16 (1970-2155 covers the practical range; year < 0 is
/// rendered as 0 to keep the field width sane).
fn ymd_from_days_since_epoch(days: i32) -> (u16, u8, u8) {
    // Re-use the engine's canonical formatter then re-parse —
    // gives one source of truth for the Y/M/D split logic.
    let text = spg_engine::eval::format_date(days);
    let bytes = text.as_bytes();
    let year: u16 = std::str::from_utf8(&bytes[..4])
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let month: u8 = std::str::from_utf8(&bytes[5..7])
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(1);
    let day: u8 = std::str::from_utf8(&bytes[8..10])
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(1);
    (year, month, day)
}

impl PreparedEntry {
    fn clone_for_exec(&self) -> Self {
        Self {
            sql: self.sql.clone(),
            columns: self.columns.clone(),
            param_count: self.param_count,
        }
    }
}

/// Parse the bind-time payload of COM_STMT_EXECUTE.
///
/// Layout (after the 9-byte header the caller consumed):
///   * `(num_params + 7) / 8` bytes — NULL bitmap
///   * 1 byte `new_params_bound_flag`
///     * == 1: `2 * num_params` bytes (1 byte type, 1 byte
///       signed) + per-value encoded data
///     * == 0: reuse last bound types (we don't cache, so this
///       surfaces as an error)
fn parse_execute_params(
    entry: &PreparedEntry,
    payload: &[u8],
) -> Result<Vec<Value<'static>>, String> {
    let n = entry.param_count as usize;
    if n == 0 {
        return Ok(Vec::new());
    }
    let null_bitmap_len = (n + 7) / 8;
    if payload.len() < null_bitmap_len + 1 {
        return Err("EXECUTE payload truncated (null bitmap)".to_string());
    }
    let null_bitmap = &payload[..null_bitmap_len];
    let mut pos = null_bitmap_len;
    let new_params_bound = payload[pos];
    pos += 1;
    if new_params_bound != 1 {
        return Err(
            "EXECUTE without re-bound types (new_params_bound_flag = 0) is not yet supported"
                .to_string(),
        );
    }
    if payload.len() < pos + 2 * n {
        return Err("EXECUTE payload truncated (param types)".to_string());
    }
    let types_start = pos;
    let mut values_pos = types_start + 2 * n;
    pos += 2 * n;
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        let is_null = (null_bitmap[i / 8] >> (i % 8)) & 1 == 1;
        if is_null {
            out.push(Value::Null);
            continue;
        }
        let ty_byte = payload[types_start + 2 * i];
        let unsigned_flag = payload[types_start + 2 * i + 1] & 0x80 != 0;
        let (value, consumed) = decode_binary_param(ty_byte, unsigned_flag, &payload[values_pos..])
            .map_err(|e| format!("param {i}: {e}"))?;
        out.push(value);
        values_pos += consumed;
        let _ = pos;
    }
    Ok(out)
}

fn decode_binary_param(
    ty: u8,
    unsigned: bool,
    buf: &[u8],
) -> Result<(Value<'static>, usize), String> {
    match ty {
        // MYSQL_TYPE_TINY
        0x01 => {
            if buf.is_empty() {
                return Err("truncated TINY".to_string());
            }
            let v = if unsigned {
                i16::from(buf[0])
            } else {
                i16::from(buf[0] as i8)
            };
            Ok((Value::SmallInt(v), 1))
        }
        // MYSQL_TYPE_SHORT / YEAR
        0x02 | 0x0d => {
            if buf.len() < 2 {
                return Err("truncated SHORT".to_string());
            }
            let v = if unsigned {
                i32::from(u16::from_le_bytes([buf[0], buf[1]]))
            } else {
                i32::from(i16::from_le_bytes([buf[0], buf[1]]))
            };
            Ok((Value::Int(v), 2))
        }
        // MYSQL_TYPE_LONG / INT24
        0x03 | 0x09 => {
            if buf.len() < 4 {
                return Err("truncated LONG".to_string());
            }
            let v = if unsigned {
                i64::from(u32::from_le_bytes(buf[..4].try_into().unwrap()))
            } else {
                i64::from(i32::from_le_bytes(buf[..4].try_into().unwrap()))
            };
            if let Ok(small) = i32::try_from(v) {
                Ok((Value::Int(small), 4))
            } else {
                Ok((Value::BigInt(v), 4))
            }
        }
        // MYSQL_TYPE_LONGLONG
        0x08 => {
            if buf.len() < 8 {
                return Err("truncated LONGLONG".to_string());
            }
            let v = if unsigned {
                i64::try_from(u64::from_le_bytes(buf[..8].try_into().unwrap()))
                    .map_err(|e| e.to_string())?
            } else {
                i64::from_le_bytes(buf[..8].try_into().unwrap())
            };
            Ok((Value::BigInt(v), 8))
        }
        // MYSQL_TYPE_FLOAT
        0x04 => {
            if buf.len() < 4 {
                return Err("truncated FLOAT".to_string());
            }
            let v = f32::from_le_bytes(buf[..4].try_into().unwrap());
            Ok((Value::Float(f64::from(v)), 4))
        }
        // MYSQL_TYPE_DOUBLE
        0x05 => {
            if buf.len() < 8 {
                return Err("truncated DOUBLE".to_string());
            }
            let v = f64::from_le_bytes(buf[..8].try_into().unwrap());
            Ok((Value::Float(v), 8))
        }
        // MYSQL_TYPE_NULL
        0x06 => Ok((Value::Null, 0)),
        // String-shaped: VAR_STRING / STRING / VARCHAR / BLOB /
        // TINY_BLOB / MEDIUM_BLOB / LONG_BLOB / TEXT / GEOMETRY /
        // DECIMAL / NEWDECIMAL / JSON / BIT
        0xfd | 0xfe | 0x0f | 0xfc | 0xfb | 0xfa | 0xf9 | 0xf8 | 0xf5 | 0xf6 | 0x00 | 0x10 => {
            let mut cursor = Cursor::new(buf);
            let n = cursor
                .lenenc_int()
                .ok_or_else(|| "truncated string lenenc".to_string())?;
            let bytes = cursor
                .bytes(n as usize)
                .ok_or_else(|| "truncated string body".to_string())?;
            let consumed = (n as usize)
                + match buf.first() {
                    Some(b) if *b < 0xfb => 1,
                    Some(0xfc) => 3,
                    Some(0xfd) => 4,
                    Some(0xfe) => 9,
                    _ => 1,
                };
            let s = String::from_utf8_lossy(&bytes).into_owned();
            Ok((Value::text(s), consumed))
        }
        other => Err(format!("unsupported binary param type 0x{other:02x}")),
    }
}

// ---- COM_FIELD_LIST -----------------------------------------

/// v7.17.0 Phase 3.P0-75 — `mysqldump --opt` uses this to
/// probe a table's column shape ahead of `SELECT * FROM <table>`.
/// Payload shape (post-cmd byte): `<table_name>\0[<wildcard>]`.
/// We ignore the wildcard (every column gets a column_def_41
/// regardless) and return either:
///   * N column_def_41 packets + EOF / OK on success
///   * ERR(1146) when the table is missing.
fn handle_com_field_list(
    stream: &mut (dyn ReadWrite + '_),
    state: &Arc<ServerState>,
    payload: &[u8],
    start_seqno: u8,
) -> std::io::Result<()> {
    let nul = payload
        .iter()
        .position(|b| *b == 0)
        .unwrap_or(payload.len());
    let table = std::str::from_utf8(&payload[..nul]).unwrap_or("");
    let cols_opt = {
        let Ok(engine) = state.engine.read() else {
            return write_packet(
                stream,
                start_seqno,
                &encode_err_packet(1815, "HY000", "engine lock poisoned"),
            );
        };
        engine
            .catalog()
            .get(table)
            .map(|t| t.schema().columns.clone())
    };
    let cols = match cols_opt {
        Some(c) => c,
        None => {
            return write_packet(
                stream,
                start_seqno,
                &encode_err_packet(1146, "42S02", &format!("Table '{table}' doesn't exist")),
            );
        }
    };
    let mut seq = start_seqno;
    for c in &cols {
        let buf = encode_column_def_41(c);
        write_packet(stream, seq, &buf)?;
        seq = seq.wrapping_add(1);
    }
    // CLIENT_DEPRECATE_EOF — trailing OK marker.
    write_packet(stream, seq, &encode_ok_packet())?;
    Ok(())
}

// ---- text-protocol result set -------------------------------

fn encode_text_result_set(
    stream: &mut (dyn ReadWrite + '_),
    columns: &[ColumnSchema],
    rows: &[spg_storage::Row],
    start_seqno: u8,
) -> std::io::Result<()> {
    // 1) column_count packet — single length-encoded integer.
    let mut payload = Vec::new();
    encode_lenenc_int(&mut payload, columns.len() as u64);
    let mut seq = start_seqno;
    write_packet(stream, seq, &payload)?;
    seq = seq.wrapping_add(1);
    // 2) column_def_41 packets — one per column.
    for c in columns {
        let buf = encode_column_def_41(c);
        write_packet(stream, seq, &buf)?;
        seq = seq.wrapping_add(1);
    }
    // 3) row packets. CLIENT_DEPRECATE_EOF means we skip the
    // intermediate EOF marker and just stream rows.
    for row in rows {
        let buf = encode_text_row(&row.values, columns);
        write_packet(stream, seq, &buf)?;
        seq = seq.wrapping_add(1);
    }
    // 4) trailing OK packet (CLIENT_DEPRECATE_EOF replaces the
    // trailing EOF with OK). status_flags + warnings.
    write_packet(stream, seq, &encode_ok_packet())?;
    Ok(())
}

fn encode_column_def_41(c: &ColumnSchema) -> Vec<u8> {
    let mut buf = Vec::with_capacity(64);
    // catalog — always "def" per MySQL spec.
    encode_lenenc_string(&mut buf, b"def");
    // schema (empty for v7.17 — no SHOW SCHEMAS dispatch here)
    encode_lenenc_string(&mut buf, b"");
    // table / org_table (empty for now — engine doesn't surface
    // the source table for projection items)
    encode_lenenc_string(&mut buf, b"");
    encode_lenenc_string(&mut buf, b"");
    // name / org_name — the projected column label.
    encode_lenenc_string(&mut buf, c.name.as_bytes());
    encode_lenenc_string(&mut buf, c.name.as_bytes());
    // 1-byte fixed-length field length marker.
    buf.push(0x0c);
    // character set. utf8mb4 = 0x002d (45) — matches what we
    // advertise in the greeting (the 0xff byte in HandshakeV10
    // is the same collation in a different namespace).
    buf.extend_from_slice(&0x002d_u16.to_le_bytes());
    // column length — set per-type, padded large enough for the
    // text representation of any value the engine could produce.
    let column_length = column_length_for(c.ty);
    buf.extend_from_slice(&column_length.to_le_bytes());
    // column type byte
    buf.push(mysql_field_type(c.ty));
    // flags: NOT NULL = 0x0001; mostly leave 0 elsewhere.
    let flags: u16 = u16::from(!c.nullable);
    buf.extend_from_slice(&flags.to_le_bytes());
    // decimals: 0x1f = "no fixed decimal point" for floats.
    let decimals: u8 = match c.ty {
        DataType::Float => 0x1f,
        DataType::Numeric { scale, .. } => scale,
        _ => 0x00,
    };
    buf.push(decimals);
    // 2-byte trailing filler.
    buf.extend_from_slice(&[0u8, 0u8]);
    buf
}

fn encode_text_row(values: &[Value], _columns: &[ColumnSchema]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(values.len() * 8);
    for v in values {
        match v {
            Value::Null => {
                // MySQL text protocol: NULL is a single byte 0xfb.
                buf.push(0xfb);
            }
            other => {
                let text = value_to_mysql_text(other);
                encode_lenenc_string(&mut buf, text.as_bytes());
            }
        }
    }
    buf
}

fn value_to_mysql_text(v: &Value) -> String {
    match v {
        Value::Null => String::new(), // handled by caller
        Value::Bool(b) => if *b { "1" } else { "0" }.to_string(),
        Value::SmallInt(n) => n.to_string(),
        Value::Int(n) => n.to_string(),
        Value::BigInt(n) => n.to_string(),
        Value::Float(f) => format!("{f}"),
        Value::Text(s) | Value::Json(s) => s.to_string(),
        Value::Date(days) => format_date_mysql(*days),
        Value::Timestamp(us) => format_timestamp_mysql(*us),
        // Other types fall through to engine's canonical text
        // renderer when SPG ships one; otherwise the Debug shape
        // keeps the wire flow alive without losing the row.
        other => format!("{other:?}"),
    }
}

fn format_date_mysql(days_since_epoch: i32) -> String {
    // `YYYY-MM-DD` per MySQL DATE text format — same shape as
    // SPG engine's canonical PG-style DATE render.
    spg_engine::eval::format_date(days_since_epoch)
}

fn format_timestamp_mysql(us: i64) -> String {
    // `YYYY-MM-DD HH:MM:SS[.ffffff]` per MySQL DATETIME text
    // format. SPG's format_timestamp produces the PG-canonical
    // `YYYY-MM-DD HH:MM:SS` form already.
    spg_engine::eval::format_timestamp(us)
}

fn mysql_field_type(ty: DataType) -> u8 {
    match ty {
        DataType::Bool => 0x01,      // MYSQL_TYPE_TINY
        DataType::SmallInt => 0x02,  // MYSQL_TYPE_SHORT
        DataType::Int => 0x03,       // MYSQL_TYPE_LONG
        DataType::BigInt => 0x08,    // MYSQL_TYPE_LONGLONG
        DataType::Float => 0x05,     // MYSQL_TYPE_DOUBLE
        DataType::Date => 0x0a,      // MYSQL_TYPE_DATE
        DataType::Timestamp => 0x07, // MYSQL_TYPE_TIMESTAMP
        DataType::Timestamptz => 0x07,
        DataType::Json | DataType::Jsonb => 0xf5, // MYSQL_TYPE_JSON
        DataType::Numeric { .. } => 0xf6,         // MYSQL_TYPE_NEWDECIMAL
        DataType::Bytes => 0xfc,                  // MYSQL_TYPE_BLOB
        // Everything else (Text, Char, Varchar, Uuid, vectors,
        // arrays, ts*, range, hstore, money, time*) decodes as a
        // varchar-string on the wire — clients see the engine's
        // canonical text rendering.
        _ => 0xfd, // MYSQL_TYPE_VAR_STRING
    }
}

fn column_length_for(ty: DataType) -> u32 {
    match ty {
        DataType::Bool => 1,
        DataType::SmallInt => 6,
        DataType::Int => 11,
        DataType::BigInt => 20,
        DataType::Float => 22,
        DataType::Date => 10,
        DataType::Timestamp | DataType::Timestamptz => 26,
        DataType::Numeric { precision, .. } => u32::from(precision) + 2,
        // Large pad for text-shaped — most clients only use this
        // as an upper bound for column display width.
        _ => 16_777_215,
    }
}

// ---- OK / lenenc helpers ------------------------------------

pub(crate) fn encode_ok_with_affected(affected: u64) -> Vec<u8> {
    let mut out = Vec::with_capacity(11);
    out.push(0x00);
    encode_lenenc_int(&mut out, affected);
    out.push(0); // last_insert_id = 0
    out.extend_from_slice(&SERVER_STATUS_AUTOCOMMIT.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes()); // warnings
    out
}

pub(crate) fn encode_lenenc_int(buf: &mut Vec<u8>, v: u64) {
    if v < 251 {
        buf.push(v as u8);
    } else if v < 1 << 16 {
        buf.push(0xfc);
        buf.extend_from_slice(&(v as u16).to_le_bytes());
    } else if v < 1 << 24 {
        buf.push(0xfd);
        let bytes = (v as u32).to_le_bytes();
        buf.extend_from_slice(&bytes[..3]);
    } else {
        buf.push(0xfe);
        buf.extend_from_slice(&v.to_le_bytes());
    }
}

pub(crate) fn encode_lenenc_string(buf: &mut Vec<u8>, bytes: &[u8]) {
    encode_lenenc_int(buf, bytes.len() as u64);
    buf.extend_from_slice(bytes);
}

/// Outcome of verifying a HandshakeResponse41 against the engine's
/// user store.
#[derive(Debug)]
enum AuthOutcome {
    Ok,
    /// v7.17.0 Phase 3.P0-72 — caching_sha2_password fast-path
    /// success. Different from `Ok` because the protocol
    /// requires an Auth More Data (0x01 0x03) packet ahead of
    /// the OK so the client knows the cache hit happened
    /// without an RSA exchange.
    CachingSha2FastAuthOk,
    AccessDenied(String),
    PluginMismatch(String),
}

fn verify_handshake_response(
    state: &Arc<ServerState>,
    response: &HandshakeResponse41,
    scramble: &[u8],
) -> AuthOutcome {
    let plugin = response
        .auth_plugin_name
        .as_deref()
        .unwrap_or(AUTH_PLUGIN_NATIVE);
    let engine = match state.engine.read() {
        Ok(e) => e,
        Err(_) => {
            return AuthOutcome::AccessDenied(
                "engine lock poisoned; refusing connection".to_string(),
            );
        }
    };
    // Open mode — no users registered → accept any plugin.
    if engine.users().is_empty() {
        return match plugin {
            AUTH_PLUGIN_NATIVE => AuthOutcome::Ok,
            AUTH_PLUGIN_CACHING_SHA2 => AuthOutcome::CachingSha2FastAuthOk,
            other => AuthOutcome::PluginMismatch(other.to_string()),
        };
    }
    let user = &response.username;
    let Some(record) = engine.users().get(user) else {
        return AuthOutcome::AccessDenied(format!("Access denied for user '{user}'"));
    };
    if response.auth_response.is_empty() {
        return AuthOutcome::AccessDenied(format!(
            "Access denied for user '{user}' (empty password)"
        ));
    }
    match plugin {
        AUTH_PLUGIN_NATIVE => {
            if record.verify_mysql_native_password(scramble, &response.auth_response) {
                AuthOutcome::Ok
            } else {
                AuthOutcome::AccessDenied(format!(
                    "Access denied for user '{user}' (using mysql_native_password)"
                ))
            }
        }
        AUTH_PLUGIN_CACHING_SHA2 => {
            if record.verify_caching_sha2_password(scramble, &response.auth_response) {
                AuthOutcome::CachingSha2FastAuthOk
            } else {
                // RSA full-auth fallback is a v7.18 carve-out —
                // surface as Access Denied with a hint.
                AuthOutcome::AccessDenied(format!(
                    "Access denied for user '{user}' (caching_sha2_password fast path failed; RSA full-auth fallback not yet implemented)"
                ))
            }
        }
        other => AuthOutcome::PluginMismatch(other.to_string()),
    }
}

/// Minimal OK packet — protocol-41 shape.
///   0x00 header + lenenc affected_rows(=0) + lenenc last_insert_id(=0)
///   + 2-byte status_flags(AUTOCOMMIT) + 2-byte warnings(=0)
pub(crate) fn encode_ok_packet() -> Vec<u8> {
    let mut out = Vec::with_capacity(7);
    out.push(0x00);
    out.push(0); // affected_rows = 0
    out.push(0); // last_insert_id = 0
    out.extend_from_slice(&SERVER_STATUS_AUTOCOMMIT.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes()); // warnings
    out
}

// ---- HandshakeV10 packet encode -----------------------------

pub(crate) struct HandshakeV10Greeting {
    pub(crate) protocol_version: u8,
    pub(crate) server_version: String,
    pub(crate) connection_id: u32,
    /// 20-byte scramble (auth-plugin-data parts 1 + 2 combined
    /// without the trailing null byte).
    pub(crate) scramble: Vec<u8>,
    pub(crate) capability_flags: u32,
    pub(crate) character_set: u8,
    pub(crate) status_flags: u16,
    pub(crate) auth_plugin_name: String,
}

pub(crate) fn encode_handshake_v10(g: &HandshakeV10Greeting) -> Vec<u8> {
    debug_assert_eq!(
        g.scramble.len(),
        20,
        "MySQL scramble is fixed at 20 bytes (8 + 12) per spec"
    );
    let mut out = Vec::with_capacity(64 + g.server_version.len() + g.auth_plugin_name.len());
    out.push(g.protocol_version);
    out.extend_from_slice(g.server_version.as_bytes());
    out.push(0); // null-terminator on server_version
    out.extend_from_slice(&g.connection_id.to_le_bytes());
    // auth-plugin-data-part-1 (8 bytes)
    out.extend_from_slice(&g.scramble[..8]);
    out.push(0); // filler
    let caps = g.capability_flags;
    // lower 2 bytes of cap flags
    out.extend_from_slice(&(caps as u16).to_le_bytes());
    out.push(g.character_set);
    out.extend_from_slice(&g.status_flags.to_le_bytes());
    // upper 2 bytes of cap flags
    out.extend_from_slice(&((caps >> 16) as u16).to_le_bytes());
    // length of full auth-plugin-data (server-side length we
    // advertise — 21 = 8 + 12 + 1 null terminator on part 2).
    out.push(21);
    // 10 reserved bytes (all zero) per spec.
    out.extend_from_slice(&[0u8; 10]);
    // auth-plugin-data-part-2: 12 bytes + 1 null terminator =
    // 13 bytes per the v10 spec ("length_of_auth_plugin_data" is
    // max(13, real_len) — we send the real 12 plus null).
    out.extend_from_slice(&g.scramble[8..20]);
    out.push(0);
    // null-terminated auth plugin name (CLIENT_PLUGIN_AUTH).
    out.extend_from_slice(g.auth_plugin_name.as_bytes());
    out.push(0);
    out
}

// ---- HandshakeResponse41 packet parse ------------------------

#[derive(Debug, Clone)]
pub(crate) struct HandshakeResponse41 {
    pub(crate) client_capabilities: u32,
    pub(crate) max_packet_size: u32,
    pub(crate) character_set: u8,
    pub(crate) username: String,
    pub(crate) auth_response: Vec<u8>,
    pub(crate) database: Option<String>,
    pub(crate) auth_plugin_name: Option<String>,
}

pub(crate) fn parse_handshake_response_41(payload: &[u8]) -> Result<HandshakeResponse41, String> {
    let mut p = Cursor::new(payload);
    let caps = p
        .u32_le()
        .ok_or_else(|| "truncated cap flags".to_string())?;
    let max_packet = p
        .u32_le()
        .ok_or_else(|| "truncated max packet size".to_string())?;
    let charset = p.u8().ok_or_else(|| "truncated charset".to_string())?;
    // 23 reserved filler bytes per spec.
    p.skip(23)
        .ok_or_else(|| "truncated reserved filler".to_string())?;
    let username = p
        .null_string()
        .ok_or_else(|| "truncated username".to_string())?;
    // Auth response: length-encoded if CLIENT_PLUGIN_AUTH_LENENC_CLIENT_DATA,
    // 1-byte length if CLIENT_SECURE_CONNECTION, NUL-terminated
    // otherwise. P0-70 supports the modern paths (lenenc + 1-byte).
    let auth_response = if caps & CLIENT_PLUGIN_AUTH_LENENC_CLIENT_DATA != 0 {
        let n = p
            .lenenc_int()
            .ok_or_else(|| "truncated auth_response lenenc".to_string())?;
        if n > 4096 {
            return Err(format!("auth_response oversized: {n} bytes"));
        }
        p.bytes(n as usize)
            .ok_or_else(|| "truncated auth_response payload".to_string())?
    } else if caps & CLIENT_SECURE_CONNECTION != 0 {
        let n = p
            .u8()
            .ok_or_else(|| "truncated auth_response u8".to_string())?;
        p.bytes(n as usize)
            .ok_or_else(|| "truncated auth_response payload".to_string())?
    } else {
        return Err("legacy CLIENT_LONG_PASSWORD auth (pre-4.1) is not supported".to_string());
    };
    let database = if caps & CLIENT_CONNECT_WITH_DB != 0 {
        Some(
            p.null_string()
                .ok_or_else(|| "truncated database name".to_string())?,
        )
    } else {
        None
    };
    let auth_plugin_name = if caps & CLIENT_PLUGIN_AUTH != 0 {
        Some(
            p.null_string()
                .ok_or_else(|| "truncated auth plugin name".to_string())?,
        )
    } else {
        None
    };
    Ok(HandshakeResponse41 {
        client_capabilities: caps,
        max_packet_size: max_packet,
        character_set: charset,
        username,
        auth_response,
        database,
        auth_plugin_name,
    })
}

// ---- ERR packet ---------------------------------------------

pub(crate) fn encode_err_packet(errno: u16, sqlstate: &str, msg: &str) -> Vec<u8> {
    debug_assert_eq!(sqlstate.len(), 5, "SQLSTATE is exactly 5 ASCII chars");
    let mut out = Vec::with_capacity(9 + msg.len());
    out.push(0xff); // ERR header
    out.extend_from_slice(&errno.to_le_bytes());
    out.push(b'#');
    out.extend_from_slice(sqlstate.as_bytes());
    out.extend_from_slice(msg.as_bytes());
    out
}

// ---- packet framing -----------------------------------------

pub(crate) fn write_packet(
    stream: &mut dyn Write,
    seqno: u8,
    payload: &[u8],
) -> std::io::Result<()> {
    if payload.len() > 0x00ff_ffff {
        // Multi-segment packets land in P0-73 (COM_QUERY can
        // send very large LOAD DATA). For P0-70 the surface is
        // tiny — ERR / handshake — so a hard error here is the
        // safe choice.
        return Err(std::io::Error::other(format!(
            "mysqlwire: refusing to send {} bytes — multi-segment send is P0-73",
            payload.len()
        )));
    }
    let len = payload.len() as u32;
    let mut hdr = [0u8; 4];
    hdr[0] = len as u8;
    hdr[1] = (len >> 8) as u8;
    hdr[2] = (len >> 16) as u8;
    hdr[3] = seqno;
    stream.write_all(&hdr)?;
    stream.write_all(payload)?;
    Ok(())
}

pub(crate) fn read_packet(stream: &mut dyn Read) -> std::io::Result<(u8, Vec<u8>)> {
    let mut hdr = [0u8; 4];
    stream.read_exact(&mut hdr)?;
    let len = u32::from(hdr[0]) | (u32::from(hdr[1]) << 8) | (u32::from(hdr[2]) << 16);
    let seqno = hdr[3];
    let mut payload = vec![0u8; len as usize];
    stream.read_exact(&mut payload)?;
    Ok((seqno, payload))
}

// ---- payload cursor helpers ---------------------------------

pub(crate) struct Cursor<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    pub(crate) fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    pub(crate) fn u8(&mut self) -> Option<u8> {
        let b = *self.buf.get(self.pos)?;
        self.pos += 1;
        Some(b)
    }

    pub(crate) fn u32_le(&mut self) -> Option<u32> {
        let slice = self.buf.get(self.pos..self.pos + 4)?;
        let v = u32::from_le_bytes(slice.try_into().ok()?);
        self.pos += 4;
        Some(v)
    }

    pub(crate) fn skip(&mut self, n: usize) -> Option<()> {
        if self.pos + n > self.buf.len() {
            return None;
        }
        self.pos += n;
        Some(())
    }

    pub(crate) fn bytes(&mut self, n: usize) -> Option<Vec<u8>> {
        let slice = self.buf.get(self.pos..self.pos + n)?;
        self.pos += n;
        Some(slice.to_vec())
    }

    pub(crate) fn null_string(&mut self) -> Option<String> {
        let rest = self.buf.get(self.pos..)?;
        let nul = rest.iter().position(|b| *b == 0)?;
        let s = String::from_utf8(rest[..nul].to_vec()).ok()?;
        self.pos += nul + 1;
        Some(s)
    }

    pub(crate) fn lenenc_int(&mut self) -> Option<u64> {
        // MySQL length-encoded integer: 0..250 = 1 byte; 0xfb =
        // NULL; 0xfc = 2-byte; 0xfd = 3-byte; 0xfe = 8-byte.
        let first = self.u8()?;
        match first {
            0xfb => Some(0), // treat NULL as zero — caller decides if NULL is legal
            0xfc => {
                let slice = self.buf.get(self.pos..self.pos + 2)?;
                let v = u16::from_le_bytes(slice.try_into().ok()?);
                self.pos += 2;
                Some(u64::from(v))
            }
            0xfd => {
                let slice = self.buf.get(self.pos..self.pos + 3)?;
                let mut bytes = [0u8; 4];
                bytes[..3].copy_from_slice(slice);
                let v = u32::from_le_bytes(bytes);
                self.pos += 3;
                Some(u64::from(v))
            }
            0xfe => {
                let slice = self.buf.get(self.pos..self.pos + 8)?;
                let v = u64::from_le_bytes(slice.try_into().ok()?);
                self.pos += 8;
                Some(v)
            }
            n => Some(u64::from(n)),
        }
    }
}

// ---- scramble / version helpers -----------------------------

fn server_version_string() -> String {
    // Advertise an 8.0 lineage so caching_sha2 capable clients
    // don't downgrade to mysql_native_password preemptively.
    // The trailing tag identifies us to dump tooling.
    format!("8.0.0-spg-v{}", env!("CARGO_PKG_VERSION"))
}

/// Generate a deterministic 20-byte scramble from a seed. xorshift64*
/// is enough randomness for the pre-auth nonce (the auth proof
/// itself binds the password); this avoids pulling `rand` and
/// keeps replays reproducible inside one process for the e2e
/// tests.
pub(crate) fn generate_scramble(seed: u32) -> Vec<u8> {
    let mut state: u64 = u64::from(seed)
        .wrapping_mul(2862933555777941757)
        .wrapping_add(3037000493);
    if state == 0 {
        state = 0x9E37_79B9_7F4A_7C15;
    }
    let mut out = Vec::with_capacity(20);
    while out.len() < 20 {
        // xorshift64*
        state ^= state >> 12;
        state ^= state << 25;
        state ^= state >> 27;
        let v = state.wrapping_mul(0x2545F4914F6CDD1D);
        for byte in v.to_le_bytes() {
            if out.len() == 20 {
                break;
            }
            // Stay in the printable ASCII range to match what
            // real MySQL servers send (and to make handshake
            // dumps human-readable in e2e logs).
            out.push(((byte % (0x7e - 0x21)) + 0x21).max(0x21));
        }
    }
    out
}

// ---- unit tests ----------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handshake_v10_round_trip_through_cursor() {
        let g = HandshakeV10Greeting {
            protocol_version: 10,
            server_version: "8.0.0-spg-vtest".to_string(),
            connection_id: 42,
            scramble: vec![b'a'; 20],
            capability_flags: SERVER_CAPABILITIES,
            character_set: CHARSET_UTF8MB4,
            status_flags: SERVER_STATUS_AUTOCOMMIT,
            auth_plugin_name: AUTH_PLUGIN_NATIVE.to_string(),
        };
        let bytes = encode_handshake_v10(&g);
        // Re-parse the byte string to make sure each field is
        // where the spec says it should be.
        let mut p = Cursor::new(&bytes);
        assert_eq!(p.u8().unwrap(), 10);
        assert_eq!(p.null_string().unwrap(), "8.0.0-spg-vtest");
        assert_eq!(p.u32_le().unwrap(), 42);
        let scramble_pt1 = p.bytes(8).unwrap();
        assert_eq!(scramble_pt1, vec![b'a'; 8]);
        assert_eq!(p.u8().unwrap(), 0); // filler
        let cap_lo = u32::from(p.u8().unwrap()) | (u32::from(p.u8().unwrap()) << 8);
        let _charset = p.u8().unwrap();
        let _status = p.u8().unwrap();
        let _status_hi = p.u8().unwrap();
        let cap_hi = u32::from(p.u8().unwrap()) | (u32::from(p.u8().unwrap()) << 8);
        let recovered_caps = cap_lo | (cap_hi << 16);
        assert_eq!(recovered_caps, SERVER_CAPABILITIES);
        assert_eq!(p.u8().unwrap(), 21); // auth_plugin_data_len
        p.skip(10).unwrap(); // reserved zeros
        let scramble_pt2 = p.bytes(12).unwrap();
        assert_eq!(scramble_pt2, vec![b'a'; 12]);
        assert_eq!(p.u8().unwrap(), 0); // null after part-2
        assert_eq!(p.null_string().unwrap(), AUTH_PLUGIN_NATIVE);
    }

    #[test]
    fn handshake_response_41_parses_minimal_payload() {
        // Build the smallest valid HandshakeResponse41 by hand:
        // CLIENT_PROTOCOL_41 + CLIENT_SECURE_CONNECTION +
        // CLIENT_PLUGIN_AUTH, no DB / no connect-attrs.
        let caps = CLIENT_PROTOCOL_41 | CLIENT_SECURE_CONNECTION | CLIENT_PLUGIN_AUTH;
        let mut payload = Vec::new();
        payload.extend_from_slice(&caps.to_le_bytes());
        payload.extend_from_slice(&16_777_215u32.to_le_bytes()); // max_packet
        payload.push(CHARSET_UTF8MB4);
        payload.extend_from_slice(&[0u8; 23]); // reserved filler
        payload.extend_from_slice(b"root\0");
        payload.push(0); // auth_response length = 0 (no password)
        payload.extend_from_slice(b"mysql_native_password\0");
        let parsed = parse_handshake_response_41(&payload).unwrap();
        assert_eq!(parsed.client_capabilities, caps);
        assert_eq!(parsed.username, "root");
        assert!(parsed.auth_response.is_empty());
        assert_eq!(
            parsed.auth_plugin_name.as_deref(),
            Some("mysql_native_password")
        );
        assert!(parsed.database.is_none());
    }

    #[test]
    fn handshake_response_41_with_db_and_password() {
        let caps = CLIENT_PROTOCOL_41
            | CLIENT_SECURE_CONNECTION
            | CLIENT_PLUGIN_AUTH
            | CLIENT_CONNECT_WITH_DB;
        let mut payload = Vec::new();
        payload.extend_from_slice(&caps.to_le_bytes());
        payload.extend_from_slice(&16_777_215u32.to_le_bytes());
        payload.push(CHARSET_UTF8MB4);
        payload.extend_from_slice(&[0u8; 23]);
        payload.extend_from_slice(b"alice\0");
        payload.push(20); // 20-byte auth_response
        payload.extend_from_slice(&[0xab; 20]);
        payload.extend_from_slice(b"mydb\0");
        payload.extend_from_slice(b"mysql_native_password\0");
        let parsed = parse_handshake_response_41(&payload).unwrap();
        assert_eq!(parsed.username, "alice");
        assert_eq!(parsed.auth_response.len(), 20);
        assert_eq!(parsed.database.as_deref(), Some("mydb"));
    }

    #[test]
    fn err_packet_layout_matches_spec() {
        let bytes = encode_err_packet(1043, "08S01", "bad handshake");
        assert_eq!(bytes[0], 0xff);
        assert_eq!(&bytes[1..3], &1043u16.to_le_bytes());
        assert_eq!(bytes[3], b'#');
        assert_eq!(&bytes[4..9], b"08S01");
        assert_eq!(&bytes[9..], b"bad handshake");
    }

    #[test]
    fn lenenc_int_decodes_all_four_widths() {
        // 1-byte
        let mut p = Cursor::new(&[42u8]);
        assert_eq!(p.lenenc_int(), Some(42));
        // 2-byte (0xfc prefix)
        let mut p = Cursor::new(&[0xfc, 0x39, 0x30]);
        assert_eq!(p.lenenc_int(), Some(12345));
        // 3-byte (0xfd prefix)
        let mut p = Cursor::new(&[0xfd, 0x40, 0xe2, 0x01]);
        assert_eq!(p.lenenc_int(), Some(123456));
        // 8-byte (0xfe prefix)
        let mut p = Cursor::new(&[0xfe, 0x10, 0x32, 0x54, 0x76, 0x98, 0xba, 0xdc, 0xfe]);
        assert_eq!(p.lenenc_int(), Some(0xfedc_ba98_7654_3210));
    }

    #[test]
    fn scramble_is_deterministic_per_seed_and_printable_ascii() {
        let s1 = generate_scramble(42);
        let s2 = generate_scramble(42);
        assert_eq!(s1, s2, "same seed → same scramble");
        assert_eq!(s1.len(), 20);
        for b in &s1 {
            assert!(
                (0x21..=0x7e).contains(b),
                "scramble byte {b:#x} outside printable ASCII"
            );
        }
        let s_other = generate_scramble(43);
        assert_ne!(s1, s_other, "different seed → different scramble");
    }
}
