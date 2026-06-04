//! v6.10.0 — WAL-as-SQL pub/sub publisher.
//!
//! SPG's WAL records are already SQL strings (the v3 record
//! format wraps `[type=auto_commit_sql][sql_text]`). v6.10.0
//! lifts that into a side-channel publisher: when
//! `SPG_PUBSUB_TARGET` is set, the server emits each committed
//! auto-commit SQL record to the target in the chosen wire
//! format. Subscribers can wire SPG's WAL into a NATS-shaped
//! message bus without an explicit logical-replication
//! subscription.
//!
//! Targets (selected at boot time):
//!
//!   - `log`  — emit `NATS PUB <subject> <bytes>\r\n<payload>\r\n`
//!     lines to stderr. Validates the protocol framing
//!     end-to-end without needing a broker; useful in tests +
//!     during operator capacity planning.
//!
//! Real-broker connectivity (`nats://host:port` TCP, TLS,
//! reconnect, INFO/CONNECT handshake, JetStream, …) is a
//! STABILITY § "Out of v6.10" carve-out — the v6.10.0 ship
//! freezes the wire framing and the publisher trait so the
//! future revisit can drop in a TCP target without touching
//! the call sites.

use std::env;
use std::sync::OnceLock;

/// Subject all WAL-as-SQL records publish to. NATS subject
/// rules: dot-separated tokens, no whitespace. `spg.wal.sql` is
/// the operator-facing default; future v6.10.x may add
/// per-publication subject routing.
pub(crate) const DEFAULT_SUBJECT: &str = "spg.wal.sql";

/// One-time-parsed pubsub configuration. None when
/// `SPG_PUBSUB_TARGET` isn't set or doesn't match a known
/// target string.
static CONFIG: OnceLock<Option<Config>> = OnceLock::new();

#[derive(Debug, Clone)]
pub(crate) struct Config {
    pub target: Target,
    pub subject: String,
}

#[derive(Debug, Clone)]
pub(crate) enum Target {
    /// `SPG_PUBSUB_TARGET=log` — emit framed PUB lines to
    /// stderr. Format matches the NATS v2 wire protocol so an
    /// operator can `grep '^PUB '` and pipe directly into
    /// `nats pub --stdin`.
    LogStderr,
}

/// Read the env once + cache. Subsequent calls are O(1).
fn config() -> Option<&'static Config> {
    CONFIG
        .get_or_init(|| {
            let target_str = env::var("SPG_PUBSUB_TARGET").ok()?;
            let target = match target_str.as_str() {
                "log" => Target::LogStderr,
                other => {
                    eprintln!(
                        "spg-server: unknown SPG_PUBSUB_TARGET={other:?}; \
                         supported: log. Disabling pubsub."
                    );
                    return None;
                }
            };
            let subject =
                env::var("SPG_PUBSUB_SUBJECT").unwrap_or_else(|_| DEFAULT_SUBJECT.to_string());
            Some(Config { target, subject })
        })
        .as_ref()
}

/// Publish one SQL record. Best-effort: failures are logged +
/// swallowed (pubsub is a side-channel; an outage must not kill
/// the primary WAL commit path). No-op when pubsub is
/// disabled — single OnceLock load.
pub(crate) fn publish_sql(sql: &str) {
    let Some(cfg) = config() else { return };
    let frame = encode_nats_pub(&cfg.subject, sql.as_bytes());
    match cfg.target {
        Target::LogStderr => {
            // Use stderr for the framed output so it stays
            // visible regardless of `SPG_LOG_FORMAT` (JSON vs
            // text). One write per record — atomic up to the
            // stdio buffer size.
            eprint!("{frame}");
        }
    }
}

/// Encode a single NATS-v2 `PUB` frame:
///
/// ```text
/// PUB <subject> <#bytes>\r\n
/// <payload>\r\n
/// ```
///
/// `subject` must be a valid NATS subject (dot-separated
/// alphanumeric tokens, optionally with `*` / `>` wildcards on
/// the subscriber side). v6.10.0 doesn't validate — caller's
/// responsibility. Real-broker TCP delivery uses this exact
/// frame; the v6.10.0 log target writes it byte-identically.
pub(crate) fn encode_nats_pub(subject: &str, payload: &[u8]) -> String {
    let mut out = String::with_capacity(16 + subject.len() + payload.len());
    out.push_str("PUB ");
    out.push_str(subject);
    out.push(' ');
    out.push_str(&payload.len().to_string());
    out.push_str("\r\n");
    // Payload is opaque bytes — UTF-8 safe SQL inserts cleanly.
    // Lossless `from_utf8_lossy` is fine here (the log target
    // already round-trips text; binary payloads land in a future
    // revisit).
    out.push_str(&String::from_utf8_lossy(payload));
    out.push_str("\r\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nats_pub_frame_shape() {
        let s = encode_nats_pub("spg.wal.sql", b"INSERT INTO t VALUES (1)");
        assert!(s.starts_with("PUB spg.wal.sql 24\r\n"));
        assert!(s.ends_with("INSERT INTO t VALUES (1)\r\n"));
    }

    #[test]
    fn nats_pub_empty_payload() {
        let s = encode_nats_pub("x", b"");
        assert_eq!(s, "PUB x 0\r\n\r\n");
    }

    #[test]
    fn nats_pub_handles_newlines_in_sql() {
        // SQL with embedded newlines stays in-payload; the
        // length prefix counts them so the broker frames
        // correctly.
        let sql = "INSERT INTO t VALUES\n(1, 'a')";
        let s = encode_nats_pub("x", sql.as_bytes());
        assert!(s.starts_with(&format!("PUB x {}\r\n", sql.len())));
        assert!(s.contains(sql));
    }
}
