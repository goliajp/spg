// pedantic doc_markdown flags every bare ident in the embedded
// wire-format spec block + several proper nouns; disabling at the
// module level keeps the spec readable.
#![allow(clippy::doc_markdown)]

//! v6.1.2 — logical-replication publication catalog.
//!
//! In-memory table of publications, owned by the engine. The
//! catalog persists across restarts via the snapshot envelope's
//! v3 trailer block (see `crate::lib::build_envelope`). WAL replay
//! also rebuilds it for free since `CREATE PUBLICATION` rides the
//! same WAL path as every other DDL.
//!
//! Per [`V6_1_DESIGN.md`] §"Architectural deliberations" #1:
//! treating `spg_publications` as a regular catalog table was
//! considered but rejected — the v6.1.2 design lands an internal
//! engine field, so the table-shape catalog stays a future-table
//! (when `SHOW PUBLICATIONS` and per-publication metadata queries
//! arrive, v6.1.3 can promote this struct to a virtual table).

use alloc::collections::BTreeMap;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use spg_sql::ast::PublicationScope;

/// On-disk scope tag — v6.1.2 only writes/reads `0` (AllTables).
/// `1` and `2` are reserved for v6.1.3 (`ForTables` /
/// `AllTablesExcept`).
const SCOPE_ALL_TABLES: u8 = 0;
const SCOPE_FOR_TABLES: u8 = 1;
const SCOPE_ALL_TABLES_EXCEPT: u8 = 2;

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Publications {
    /// Insertion-ordered for deterministic snapshot output. BTreeMap
    /// orders alphabetically which is also deterministic.
    inner: BTreeMap<String, PublicationScope>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum PublicationError {
    DuplicateName(String),
    /// v6.1.2 raises this only for malformed deserialise input.
    /// (The DROP path does NOT error on a missing publication —
    /// PG-compatible silent no-op, returned by `Publications::drop`.)
    Corrupt(String),
}

impl Publications {
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

    /// Iterate `(name, scope)` in deterministic (alphabetical)
    /// order. The order matters for snapshot byte-stability.
    pub fn iter(&self) -> impl Iterator<Item = (&String, &PublicationScope)> {
        self.inner.iter()
    }

    /// PG-incompatible loud error on duplicate (PG silently does
    /// nothing on `IF NOT EXISTS`; bare `CREATE PUBLICATION` on an
    /// existing name DOES error in PG, so we match that).
    pub fn create(
        &mut self,
        name: String,
        scope: PublicationScope,
    ) -> Result<(), PublicationError> {
        if self.inner.contains_key(&name) {
            return Err(PublicationError::DuplicateName(name));
        }
        self.inner.insert(name, scope);
        Ok(())
    }

    /// Returns whether the publication was actually present. Callers
    /// can choose to surface the no-op or stay silent — the v6.1.2
    /// PG-compat policy is silent (no-op), so the engine ignores
    /// this return.
    pub fn drop(&mut self, name: &str) -> bool {
        self.inner.remove(name).is_some()
    }

    // ── serialisation (envelope v3 trailer) ─────────────────────

    /// Format:
    ///   [u16 num_publications]
    ///   for each:
    ///     [u16 name_len][name bytes]
    ///     [u8 scope_tag]
    ///       0 → AllTables (no trailer)
    ///       1 → ForTables / 2 → AllTablesExcept
    ///         [u16 num_tables]
    ///         for each: [u16 t_len][t bytes]
    pub fn serialize(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(2 + self.inner.len() * 16);
        let n = u16::try_from(self.inner.len()).expect("≤ 65,535 publications per cluster");
        out.extend_from_slice(&n.to_le_bytes());
        for (name, scope) in &self.inner {
            write_str(&mut out, name);
            match scope {
                PublicationScope::AllTables => out.push(SCOPE_ALL_TABLES),
                PublicationScope::ForTables(ts) => {
                    out.push(SCOPE_FOR_TABLES);
                    write_table_list(&mut out, ts);
                }
                PublicationScope::AllTablesExcept(ts) => {
                    out.push(SCOPE_ALL_TABLES_EXCEPT);
                    write_table_list(&mut out, ts);
                }
            }
        }
        out
    }

    pub fn deserialize(buf: &[u8]) -> Result<Self, PublicationError> {
        let mut p = 0usize;
        let n = read_u16(buf, &mut p)? as usize;
        let mut inner = BTreeMap::new();
        for _ in 0..n {
            let name = read_str(buf, &mut p)?;
            let tag = read_u8(buf, &mut p)?;
            let scope = match tag {
                SCOPE_ALL_TABLES => PublicationScope::AllTables,
                SCOPE_FOR_TABLES => PublicationScope::ForTables(read_table_list(buf, &mut p)?),
                SCOPE_ALL_TABLES_EXCEPT => {
                    PublicationScope::AllTablesExcept(read_table_list(buf, &mut p)?)
                }
                other => {
                    return Err(PublicationError::Corrupt(alloc::format!(
                        "unknown publication scope tag {other:#x}"
                    )));
                }
            };
            if inner.insert(name.clone(), scope).is_some() {
                return Err(PublicationError::Corrupt(alloc::format!(
                    "duplicate publication name {name:?} in serialised payload"
                )));
            }
        }
        if p != buf.len() {
            return Err(PublicationError::Corrupt(alloc::format!(
                "trailing bytes in publications payload: read {p}, len {}",
                buf.len()
            )));
        }
        Ok(Self { inner })
    }
}

fn write_str(out: &mut Vec<u8>, s: &str) {
    let n = u16::try_from(s.len()).expect("publication / table name fits in u16");
    out.extend_from_slice(&n.to_le_bytes());
    out.extend_from_slice(s.as_bytes());
}

fn write_table_list(out: &mut Vec<u8>, ts: &[String]) {
    let n = u16::try_from(ts.len()).expect("≤ 65,535 tables per publication");
    out.extend_from_slice(&n.to_le_bytes());
    for t in ts {
        write_str(out, t);
    }
}

fn read_u8(buf: &[u8], p: &mut usize) -> Result<u8, PublicationError> {
    let v = buf
        .get(*p)
        .copied()
        .ok_or_else(|| PublicationError::Corrupt("short read (u8)".to_string()))?;
    *p += 1;
    Ok(v)
}

fn read_u16(buf: &[u8], p: &mut usize) -> Result<u16, PublicationError> {
    let slice = buf
        .get(*p..*p + 2)
        .ok_or_else(|| PublicationError::Corrupt("short read (u16)".to_string()))?;
    let arr: [u8; 2] = slice
        .try_into()
        .map_err(|_| PublicationError::Corrupt("u16 slice".to_string()))?;
    *p += 2;
    Ok(u16::from_le_bytes(arr))
}

fn read_str(buf: &[u8], p: &mut usize) -> Result<String, PublicationError> {
    let n = read_u16(buf, p)? as usize;
    let slice = buf
        .get(*p..*p + n)
        .ok_or_else(|| PublicationError::Corrupt(alloc::format!("short read (str, {n} bytes)")))?;
    *p += n;
    core::str::from_utf8(slice)
        .map(ToString::to_string)
        .map_err(|e| PublicationError::Corrupt(alloc::format!("non-UTF-8 str: {e}")))
}

fn read_table_list(buf: &[u8], p: &mut usize) -> Result<Vec<String>, PublicationError> {
    let n = read_u16(buf, p)? as usize;
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        out.push(read_str(buf, p)?);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_roundtrips() {
        let p = Publications::new();
        let bytes = p.serialize();
        let p2 = Publications::deserialize(&bytes).unwrap();
        assert_eq!(p, p2);
    }

    #[test]
    fn single_all_tables_roundtrips() {
        let mut p = Publications::new();
        p.create("pub_a".into(), PublicationScope::AllTables).unwrap();
        let bytes = p.serialize();
        let p2 = Publications::deserialize(&bytes).unwrap();
        assert_eq!(p, p2);
        assert!(p2.contains("pub_a"));
        assert_eq!(p2.len(), 1);
    }

    #[test]
    fn duplicate_create_errors() {
        let mut p = Publications::new();
        p.create("pub_a".into(), PublicationScope::AllTables).unwrap();
        let err = p
            .create("pub_a".into(), PublicationScope::AllTables)
            .unwrap_err();
        assert_eq!(err, PublicationError::DuplicateName("pub_a".into()));
    }

    #[test]
    fn drop_present_returns_true_drop_absent_false() {
        let mut p = Publications::new();
        p.create("pub_a".into(), PublicationScope::AllTables).unwrap();
        assert!(p.drop("pub_a"));
        assert!(!p.drop("pub_a"));
        assert!(!p.drop("never_existed"));
    }

    // v6.1.3 scope variants — the on-disk shape already supports
    // them; build them by hand to lock the wire format down so the
    // v6.1.3 diff stays parser-only.
    #[test]
    fn for_tables_scope_roundtrips() {
        let mut p = Publications::new();
        p.create(
            "p_pick".into(),
            PublicationScope::ForTables(alloc::vec!["t1".into(), "t2".into()]),
        )
        .unwrap();
        let bytes = p.serialize();
        let p2 = Publications::deserialize(&bytes).unwrap();
        assert_eq!(p, p2);
    }

    #[test]
    fn all_tables_except_scope_roundtrips() {
        let mut p = Publications::new();
        p.create(
            "p_neg".into(),
            PublicationScope::AllTablesExcept(alloc::vec!["t3".into()]),
        )
        .unwrap();
        let bytes = p.serialize();
        let p2 = Publications::deserialize(&bytes).unwrap();
        assert_eq!(p, p2);
    }

    #[test]
    fn corrupt_tag_errors() {
        // Forge a single-publication payload with a bogus scope tag.
        let mut buf = Vec::new();
        buf.extend_from_slice(&1u16.to_le_bytes()); // n = 1
        buf.extend_from_slice(&3u16.to_le_bytes()); // name len = 3
        buf.extend_from_slice(b"bad");
        buf.push(0xFF); // unknown scope tag
        let err = Publications::deserialize(&buf).unwrap_err();
        assert!(matches!(err, PublicationError::Corrupt(_)));
    }

    #[test]
    fn trailing_bytes_errors() {
        let mut p = Publications::new();
        p.create("pub_a".into(), PublicationScope::AllTables).unwrap();
        let mut bytes = p.serialize();
        bytes.push(0xCC);
        let err = Publications::deserialize(&bytes).unwrap_err();
        assert!(matches!(err, PublicationError::Corrupt(_)));
    }

    #[test]
    fn deterministic_order_independent_of_insert_sequence() {
        // Same set, two insertion orders → byte-identical serialise.
        let mut p1 = Publications::new();
        p1.create("z".into(), PublicationScope::AllTables).unwrap();
        p1.create("a".into(), PublicationScope::AllTables).unwrap();
        let mut p2 = Publications::new();
        p2.create("a".into(), PublicationScope::AllTables).unwrap();
        p2.create("z".into(), PublicationScope::AllTables).unwrap();
        assert_eq!(p1.serialize(), p2.serialize());
    }
}
