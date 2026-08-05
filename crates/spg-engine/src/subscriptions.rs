// pedantic doc_markdown flags the embedded wire-format spec block
// and a handful of proper nouns; allowing at the module level
// keeps the spec readable.
#![allow(clippy::doc_markdown)]

//! v6.1.4 — logical-replication subscription catalog.
//!
//! In-memory table of subscriptions, owned by the engine. The
//! catalog persists across restarts via the snapshot envelope's
//! v4 trailer block (see `crate::lib::build_envelope`) — same
//! mechanism v6.1.2 added for publications, just an extra section.
//!
//! Subscriptions are the receive side of logical replication. A
//! `CreateSubscription` row holds:
//!   - `name`              the local identifier
//!   - `conn_str`          PG keyword=value string the worker
//!                         parses for `host=…` and `port=…`
//!   - `publications`      list of remote publication names
//!   - `enabled`           v6.1.4 hard-codes to `true`; ALTER
//!                         SUBSCRIPTION ENABLE / DISABLE lands
//!                         in a future sub-version
//!   - `last_received_pos` master-WAL byte offset the worker has
//!                         applied through (updated live by the
//!                         worker, persisted at the next snapshot)
//!
//! The worker itself lives in `spg-server::replication::
//! run_subscription_worker` — the engine layer only owns the
//! catalog state, snapshots, and answers `SHOW SUBSCRIPTIONS`.

use alloc::collections::BTreeMap;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use spg_sql::ast::CreateSubscriptionStatement;
use spg_storage::{ColumnSchema, DataType, Row, Value};

use crate::{Engine, EngineError, QueryResult};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Subscription {
    pub conn_str: String,
    pub publications: Vec<String>,
    pub enabled: bool,
    pub last_received_pos: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Subscriptions {
    inner: BTreeMap<String, Subscription>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum SubscriptionError {
    DuplicateName(String),
    Corrupt(String),
}

impl Subscriptions {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.inner.len()
    }

    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    pub fn contains(&self, name: &str) -> bool {
        self.inner.contains_key(name)
    }

    pub fn get(&self, name: &str) -> Option<&Subscription> {
        self.inner.get(name)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&String, &Subscription)> {
        self.inner.iter()
    }

    pub fn create(&mut self, name: String, sub: Subscription) -> Result<(), SubscriptionError> {
        if self.inner.contains_key(&name) {
            return Err(SubscriptionError::DuplicateName(name));
        }
        self.inner.insert(name, sub);
        Ok(())
    }

    pub fn drop(&mut self, name: &str) -> bool {
        self.inner.remove(name).is_some()
    }

    /// v6.1.4 — update the worker's last-applied master-WAL
    /// offset. Called by the subscription worker after each apply
    /// batch. Returns false when the subscription was dropped
    /// between when the worker fetched the record and when this
    /// call landed (so the worker can shut down cleanly).
    pub fn update_last_received_pos(&mut self, name: &str, pos: u64) -> bool {
        if let Some(s) = self.inner.get_mut(name) {
            // Monotone: ignore stale updates (a future restart
            // resuming from a sidecar may send an older pos than
            // the live worker has already passed).
            if pos > s.last_received_pos {
                s.last_received_pos = pos;
            }
            true
        } else {
            false
        }
    }

    // ── serialisation (envelope v4 trailer) ─────────────────────

    /// Format:
    ///   [u16 num_subscriptions]
    ///   for each:
    ///     [u16 name_len][name bytes]
    ///     [u32 conn_str_len][conn_str bytes]
    ///     [u16 num_pubs]
    ///     for each: [u16 p_len][p bytes]
    ///     [u8 enabled]
    ///     [u64 last_received_pos]
    pub fn serialize(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(2 + self.inner.len() * 64);
        let n = u16::try_from(self.inner.len()).expect("≤ 65,535 subscriptions per cluster");
        out.extend_from_slice(&n.to_le_bytes());
        for (name, sub) in &self.inner {
            write_short_str(&mut out, name);
            write_long_str(&mut out, &sub.conn_str);
            let np = u16::try_from(sub.publications.len())
                .expect("≤ 65,535 publications per subscription");
            out.extend_from_slice(&np.to_le_bytes());
            for p in &sub.publications {
                write_short_str(&mut out, p);
            }
            out.push(u8::from(sub.enabled));
            out.extend_from_slice(&sub.last_received_pos.to_le_bytes());
        }
        out
    }

    pub fn deserialize(buf: &[u8]) -> Result<Self, SubscriptionError> {
        let mut p = 0usize;
        let n = read_u16(buf, &mut p)? as usize;
        let mut inner = BTreeMap::new();
        for _ in 0..n {
            let name = read_short_str(buf, &mut p)?;
            let conn_str = read_long_str(buf, &mut p)?;
            let np = read_u16(buf, &mut p)? as usize;
            let mut publications = Vec::with_capacity(np);
            for _ in 0..np {
                publications.push(read_short_str(buf, &mut p)?);
            }
            let enabled_byte = read_u8(buf, &mut p)?;
            let enabled = match enabled_byte {
                0 => false,
                1 => true,
                other => {
                    return Err(SubscriptionError::Corrupt(alloc::format!(
                        "invalid `enabled` byte {other}, expected 0 or 1"
                    )));
                }
            };
            let last_received_pos = read_u64(buf, &mut p)?;
            if inner
                .insert(
                    name.clone(),
                    Subscription {
                        conn_str,
                        publications,
                        enabled,
                        last_received_pos,
                    },
                )
                .is_some()
            {
                return Err(SubscriptionError::Corrupt(alloc::format!(
                    "duplicate subscription name {name:?} in serialised payload"
                )));
            }
        }
        if p != buf.len() {
            return Err(SubscriptionError::Corrupt(alloc::format!(
                "trailing bytes in subscriptions payload: read {p}, len {}",
                buf.len()
            )));
        }
        Ok(Self { inner })
    }
}

fn write_short_str(out: &mut Vec<u8>, s: &str) {
    let n = u16::try_from(s.len()).expect("subscription / publication name fits in u16");
    out.extend_from_slice(&n.to_le_bytes());
    out.extend_from_slice(s.as_bytes());
}

fn write_long_str(out: &mut Vec<u8>, s: &str) {
    // conn_str may be up to a few hundred bytes; u32 keeps headroom.
    let n = u32::try_from(s.len()).expect("conn_str fits in u32");
    out.extend_from_slice(&n.to_le_bytes());
    out.extend_from_slice(s.as_bytes());
}

fn read_u8(buf: &[u8], p: &mut usize) -> Result<u8, SubscriptionError> {
    let v = buf
        .get(*p)
        .copied()
        .ok_or_else(|| SubscriptionError::Corrupt("short read (u8)".to_string()))?;
    *p += 1;
    Ok(v)
}

fn read_u16(buf: &[u8], p: &mut usize) -> Result<u16, SubscriptionError> {
    let slice = buf
        .get(*p..*p + 2)
        .ok_or_else(|| SubscriptionError::Corrupt("short read (u16)".to_string()))?;
    let arr: [u8; 2] = slice
        .try_into()
        .map_err(|_| SubscriptionError::Corrupt("u16 slice".to_string()))?;
    *p += 2;
    Ok(u16::from_le_bytes(arr))
}

fn read_u32_as_usize(buf: &[u8], p: &mut usize) -> Result<usize, SubscriptionError> {
    let slice = buf
        .get(*p..*p + 4)
        .ok_or_else(|| SubscriptionError::Corrupt("short read (u32)".to_string()))?;
    let arr: [u8; 4] = slice
        .try_into()
        .map_err(|_| SubscriptionError::Corrupt("u32 slice".to_string()))?;
    *p += 4;
    Ok(u32::from_le_bytes(arr) as usize)
}

fn read_u64(buf: &[u8], p: &mut usize) -> Result<u64, SubscriptionError> {
    let slice = buf
        .get(*p..*p + 8)
        .ok_or_else(|| SubscriptionError::Corrupt("short read (u64)".to_string()))?;
    let arr: [u8; 8] = slice
        .try_into()
        .map_err(|_| SubscriptionError::Corrupt("u64 slice".to_string()))?;
    *p += 8;
    Ok(u64::from_le_bytes(arr))
}

fn read_short_str(buf: &[u8], p: &mut usize) -> Result<String, SubscriptionError> {
    let n = read_u16(buf, p)? as usize;
    let slice = buf.get(*p..*p + n).ok_or_else(|| {
        SubscriptionError::Corrupt(alloc::format!("short read (short str, {n} bytes)"))
    })?;
    *p += n;
    core::str::from_utf8(slice)
        .map(ToString::to_string)
        .map_err(|e| SubscriptionError::Corrupt(alloc::format!("non-UTF-8 str: {e}")))
}

fn read_long_str(buf: &[u8], p: &mut usize) -> Result<String, SubscriptionError> {
    let n = read_u32_as_usize(buf, p)?;
    let slice = buf.get(*p..*p + n).ok_or_else(|| {
        SubscriptionError::Corrupt(alloc::format!("short read (long str, {n} bytes)"))
    })?;
    *p += n;
    core::str::from_utf8(slice)
        .map(ToString::to_string)
        .map_err(|e| SubscriptionError::Corrupt(alloc::format!("non-UTF-8 conn_str: {e}")))
}

impl Engine {
    /// v6.1.4 — `SHOW SUBSCRIPTIONS` row materialisation. Returns
    /// `(name, conn_str, publications, enabled, last_received_pos)`
    /// ordered by subscription name. The `publications` column is
    /// the comma-joined list ("p1, p2") for ergonomic SHOW output;
    /// callers wanting structured access read `Engine::subscriptions`.
    pub(crate) fn exec_show_subscriptions(&self) -> QueryResult {
        let columns = alloc::vec![
            ColumnSchema::new("name", DataType::Text, false),
            ColumnSchema::new("conn_str", DataType::Text, false),
            ColumnSchema::new("publications", DataType::Text, false),
            ColumnSchema::new("enabled", DataType::Bool, false),
            ColumnSchema::new("last_received_pos", DataType::BigInt, false),
        ];
        let rows: Vec<Row<'static>> = self
            .subscriptions
            .iter()
            .map(|(name, sub)| {
                Row::new(alloc::vec![
                    Value::text(name.clone()),
                    Value::text(sub.conn_str.clone()),
                    Value::text(sub.publications.join(", ")),
                    Value::Bool(sub.enabled),
                    Value::BigInt(i64::try_from(sub.last_received_pos).unwrap_or(i64::MAX)),
                ])
            })
            .collect();
        QueryResult::Rows { columns, rows }
    }

    /// v6.1.4 — `CREATE SUBSCRIPTION` runtime path. Defaults
    /// `enabled = true` and `last_received_pos = 0` for a freshly-
    /// created subscription. The actual worker thread is spawned
    /// by spg-server once the engine returns success.
    pub(crate) fn exec_create_subscription(
        &mut self,
        s: CreateSubscriptionStatement,
    ) -> Result<QueryResult, EngineError> {
        // See exec_create_publication — the in_transaction gate
        // was over-cautious; the auto-commit wrap path holds an
        // internal TX that this check was incorrectly blocking.
        let sub = Subscription {
            conn_str: s.conn_str,
            publications: s.publications,
            enabled: true,
            last_received_pos: 0,
        };
        self.subscriptions
            .create(s.name, sub)
            .map_err(|e| EngineError::Unsupported(alloc::format!("CREATE SUBSCRIPTION: {e:?}")))?;
        Ok(QueryResult::CommandOk {
            affected: 1,
            modified_catalog: true,
        })
    }

    /// v6.1.4 — `DROP SUBSCRIPTION`. Silent no-op when the name
    /// doesn't exist (PG-compatible). The associated worker is
    /// torn down by spg-server when it observes the catalog
    /// change at the next snapshot or via the engine's
    /// subscriptions accessor (the worker polls the catalog on
    /// reconnect; v6.1.5's filter-side will tighten this to an
    /// explicit signal).
    pub(crate) fn exec_drop_subscription(
        &mut self,
        name: &str,
        if_exists: bool,
    ) -> Result<QueryResult, EngineError> {
        let removed = self.subscriptions.drop(name);
        if !removed && !if_exists {
            return Err(EngineError::Unsupported(alloc::format!(
                "subscription \"{name}\" does not exist"
            )));
        }
        Ok(QueryResult::CommandOk {
            affected: usize::from(removed),
            modified_catalog: removed,
        })
    }

    /// v6.1.4 — read access to the subscription catalog. Used by
    /// the subscription worker (read its own row to find its
    /// publications + last applied position), by SHOW SUBSCRIPTIONS,
    /// and by e2e tests asserting state directly.
    pub const fn subscriptions(&self) -> &Subscriptions {
        &self.subscriptions
    }

    /// v6.1.4 — write access to `last_received_pos`. Worker
    /// calls this after each apply batch (under the engine's
    /// write-lock). Returns `false` when the subscription was
    /// dropped between when the worker received the record and
    /// when this call landed.
    pub fn subscription_advance(&mut self, name: &str, pos: u64) -> bool {
        self.subscriptions.update_last_received_pos(name, pos)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mk(
        name: &str,
        host: &str,
        pubs: &[&str],
        enabled: bool,
        pos: u64,
    ) -> (String, Subscription) {
        (
            name.to_string(),
            Subscription {
                conn_str: alloc::format!("host=127.0.0.1 port={host}"),
                publications: pubs.iter().map(|s| (*s).to_string()).collect(),
                enabled,
                last_received_pos: pos,
            },
        )
    }

    #[test]
    fn empty_roundtrips() {
        let s = Subscriptions::new();
        let bytes = s.serialize();
        assert_eq!(Subscriptions::deserialize(&bytes).unwrap(), s);
    }

    #[test]
    fn single_subscription_roundtrips() {
        let mut s = Subscriptions::new();
        let (n, sub) = mk("sub_a", "20002", &["pub_a"], true, 0);
        s.create(n, sub).unwrap();
        let bytes = s.serialize();
        let s2 = Subscriptions::deserialize(&bytes).unwrap();
        assert_eq!(s2, s);
        assert!(s2.contains("sub_a"));
    }

    #[test]
    fn multi_publication_roundtrips_with_nontrivial_last_pos() {
        let mut s = Subscriptions::new();
        let (n, sub) = mk("sub_z", "20002", &["p1", "p2", "p3"], true, 1_234_567_890);
        s.create(n, sub).unwrap();
        let s2 = Subscriptions::deserialize(&s.serialize()).unwrap();
        assert_eq!(s2, s);
        let r = s2.get("sub_z").unwrap();
        assert_eq!(r.publications, alloc::vec!["p1", "p2", "p3"]);
        assert_eq!(r.last_received_pos, 1_234_567_890);
    }

    #[test]
    fn disabled_roundtrips() {
        let mut s = Subscriptions::new();
        let (n, sub) = mk("sub_off", "20002", &["pub_a"], false, 42);
        s.create(n, sub).unwrap();
        let s2 = Subscriptions::deserialize(&s.serialize()).unwrap();
        assert!(!s2.get("sub_off").unwrap().enabled);
    }

    #[test]
    fn duplicate_name_errors() {
        let mut s = Subscriptions::new();
        let (n1, sub1) = mk("sub_a", "20002", &["pub_a"], true, 0);
        s.create(n1, sub1).unwrap();
        let (n2, sub2) = mk("sub_a", "20003", &["pub_b"], true, 0);
        assert_eq!(
            s.create(n2, sub2).unwrap_err(),
            SubscriptionError::DuplicateName("sub_a".into())
        );
    }

    #[test]
    fn drop_present_and_absent() {
        let mut s = Subscriptions::new();
        let (n, sub) = mk("sub_a", "20002", &["pub_a"], true, 0);
        s.create(n, sub).unwrap();
        assert!(s.drop("sub_a"));
        assert!(!s.drop("sub_a"));
        assert!(!s.drop("never"));
    }

    #[test]
    fn update_last_pos_monotone_and_absent_returns_false() {
        let mut s = Subscriptions::new();
        let (n, sub) = mk("sub_a", "20002", &["pub_a"], true, 100);
        s.create(n, sub).unwrap();
        assert!(s.update_last_received_pos("sub_a", 50)); // ignored (older)
        assert_eq!(s.get("sub_a").unwrap().last_received_pos, 100);
        assert!(s.update_last_received_pos("sub_a", 200));
        assert_eq!(s.get("sub_a").unwrap().last_received_pos, 200);
        assert!(!s.update_last_received_pos("missing", 1));
    }

    #[test]
    fn corrupt_enabled_byte_errors() {
        // Forge a payload with an invalid enabled byte (2).
        let mut buf = Vec::new();
        buf.extend_from_slice(&1u16.to_le_bytes()); // n = 1
        // name
        buf.extend_from_slice(&3u16.to_le_bytes());
        buf.extend_from_slice(b"bad");
        // conn_str
        buf.extend_from_slice(&0u32.to_le_bytes()); // empty
        // pubs (zero)
        buf.extend_from_slice(&0u16.to_le_bytes());
        // bogus enabled
        buf.push(2);
        // last_received_pos
        buf.extend_from_slice(&0u64.to_le_bytes());
        let err = Subscriptions::deserialize(&buf).unwrap_err();
        assert!(matches!(err, SubscriptionError::Corrupt(_)));
    }

    #[test]
    fn deterministic_order_independent_of_insert_sequence() {
        let mut s1 = Subscriptions::new();
        let (n, sub) = mk("z", "20002", &["p1"], true, 0);
        s1.create(n, sub).unwrap();
        let (n, sub) = mk("a", "20003", &["p2"], true, 0);
        s1.create(n, sub).unwrap();
        let mut s2 = Subscriptions::new();
        let (n, sub) = mk("a", "20003", &["p2"], true, 0);
        s2.create(n, sub).unwrap();
        let (n, sub) = mk("z", "20002", &["p1"], true, 0);
        s2.create(n, sub).unwrap();
        assert_eq!(s1.serialize(), s2.serialize());
    }
}
