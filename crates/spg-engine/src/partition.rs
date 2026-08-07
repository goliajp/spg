//! v7.37.6-B(sentori Epic 2 P0)— declarative partition helpers.
//!
//! Shared between `ddl.rs`(CREATE TABLE parent / child, CREATE INDEX
//! fan-out, DROP TABLE parent guard), `dml.rs`(INSERT routing), and
//! `select.rs`(planner pruning). The catalog stores partition
//! metadata on `TableSchema.partition_role`; this module provides the
//! engine-side primitives that read it and act on it.

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use spg_sql::ast::Expr;
use spg_storage::{Catalog, PartitionBound, PartitionRole, Value};

use crate::EngineError;
use crate::conversions::literal_expr_to_value;

/// True when `table_name`'s schema carries `PartitionRole::Parent`.
/// v7.39 (round 645) — is this relation the parent of anything, by
/// either mechanism?
///
/// A declarative partition parent says so in its OWN schema. An
/// inheritance parent does not: it is an ordinary table, and the only
/// record of the relationship lives in the children. So this asks the
/// children, and it is the question the FROM-clause fan-out actually
/// wants — `is_partition_parent` answers a narrower one.
pub(crate) fn has_children(catalog: &Catalog, table_name: &str) -> bool {
    is_partition_parent(catalog, table_name) || !children_of_parent(catalog, table_name).is_empty()
}

/// v7.39 (round 645) — does this relation have INHERITANCE children
/// specifically? They differ from partitions in three ways that the
/// engine has to keep apart: the parent holds rows of its own, an
/// INSERT into it does not route, and dropping it without CASCADE is an
/// error rather than a cascade.
pub(crate) fn has_inheritance_children(catalog: &Catalog, table_name: &str) -> bool {
    children_of_parent(catalog, table_name).iter().any(|c| {
        catalog.get(c).is_some_and(|t| {
            matches!(
                t.schema().partition_role,
                Some(PartitionRole::Inherits { .. })
            )
        })
    })
}

pub(crate) fn is_partition_parent(catalog: &Catalog, table_name: &str) -> bool {
    catalog
        .get(table_name)
        .map(|t| {
            matches!(
                t.schema().partition_role,
                Some(PartitionRole::Parent { .. })
            )
        })
        .unwrap_or(false)
}

/// Names of every child(`Range` + `Default`)whose `parent_name`
/// matches `parent`. Returned in catalog insertion order so the
/// caller's child-loop order is deterministic.
pub(crate) fn children_of_parent(catalog: &Catalog, parent: &str) -> Vec<String> {
    let mut out = Vec::new();
    for name in catalog.table_names() {
        let Some(t) = catalog.get(&name) else {
            continue;
        };
        match &t.schema().partition_role {
            Some(PartitionRole::Range { parent_name, .. })
            | Some(PartitionRole::Default { parent_name })
            | Some(PartitionRole::List { parent_name, .. })
            | Some(PartitionRole::Hash { parent_name, .. })
                if parent_name == parent =>
            {
                out.push(name);
            }
            // v7.39 (round 645) — an inheritance child names one or more
            // parents; it is a child of each of them.
            Some(PartitionRole::Inherits { parent_names })
                if parent_names.iter().any(|p| p == parent) =>
            {
                out.push(name);
            }
            _ => {}
        }
    }
    out
}

/// Evaluate a `PARTITION OF … FOR VALUES FROM (expr) TO (expr)`
/// boundary expression into a [`PartitionBound`].
///
/// v7.37.6-B locks the key column to TIMESTAMPTZ — every accepted
/// bound therefore resolves to either `MinValue` / `MaxValue`(the
/// PG sentinel keywords, which the parser surfaced as no-arg
/// `FunctionCall { name: "MINVALUE" / "MAXVALUE" }` markers)or an
/// `i64` microseconds-since-epoch literal coerced from a TIMESTAMPTZ
/// (text) literal. Any other shape yields `EngineError::Unsupported`.
pub(crate) fn evaluate_partition_bound(expr: Expr) -> Result<PartitionBound, EngineError> {
    if let Expr::FunctionCall { name, args } = &expr
        && args.is_empty()
    {
        let upper = name.to_ascii_uppercase();
        if upper == "MINVALUE" {
            return Ok(PartitionBound::MinValue);
        }
        if upper == "MAXVALUE" {
            return Ok(PartitionBound::MaxValue);
        }
    }
    let value = literal_expr_to_value(expr)?;
    match value {
        // TIMESTAMP and TIMESTAMPTZ share `Value::Timestamp(i64)` —
        // they only differ via `DataType` at the column level.
        Value::Timestamp(micros) => Ok(PartitionBound::TimestampTz(micros)),
        Value::Date(days) => Ok(PartitionBound::Date(days)),
        Value::BigInt(n) => Ok(PartitionBound::BigInt(n)),
        Value::Int(n) => Ok(PartitionBound::Int(n)),
        Value::SmallInt(n) => Ok(PartitionBound::SmallInt(n)),
        Value::Text(s) => {
            // Text literal: ambiguous — could be a TIMESTAMPTZ
            // string-folded literal (Range tail), or a literal TEXT
            // value (LIST tail). Try TIMESTAMPTZ parse first; if it
            // fails, fall through to TEXT membership.
            match crate::eval::parse_timestamp_literal(&s) {
                Some(micros) => Ok(PartitionBound::TimestampTz(micros)),
                None => Ok(PartitionBound::Text(s.into_owned())),
            }
        }
        other => Err(EngineError::Unsupported(format!(
            "PARTITION OF: bound must be a typed literal or \
             MINVALUE/MAXVALUE, got {other:?}"
        ))),
    }
}

/// Range bound order, used by overlap detection and INSERT routing.
/// `MinValue` is below every TIMESTAMPTZ; `MaxValue` is above every
/// TIMESTAMPTZ. Two `TimestampTz` micros compare as `i64`.
/// The i64-comparable payload of an ordered bound, or `None` for the
/// sentinels / TEXT (handled separately). All integer-family, DATE and
/// TIMESTAMPTZ bounds share one i64 ordering space; a partition's key
/// column has a single type, so its bounds are homogeneous and the shared
/// space never mixes e.g. DATE-vs-TIMESTAMPTZ. Int-literal-into-BIGINT-key
/// (Int vs BigInt) is the one legitimate cross-variant compare, and both
/// widen to the same i64.
fn bound_i64(b: &PartitionBound) -> Option<i64> {
    match b {
        PartitionBound::TimestampTz(m) => Some(*m),
        PartitionBound::BigInt(n) => Some(*n),
        PartitionBound::Int(n) => Some(i64::from(*n)),
        PartitionBound::SmallInt(n) => Some(i64::from(*n)),
        PartitionBound::Date(d) => Some(i64::from(*d)),
        PartitionBound::MinValue | PartitionBound::MaxValue | PartitionBound::Text(_) => None,
    }
}

/// v7.37 D.45 — build a range bound from a routed row's partition-key
/// value. Mirrors `evaluate_partition_bound`'s literal→bound mapping.
pub(crate) fn value_to_bound(v: &spg_storage::Value) -> Option<PartitionBound> {
    use spg_storage::Value;
    match v {
        Value::Timestamp(m) => Some(PartitionBound::TimestampTz(*m)),
        Value::BigInt(n) => Some(PartitionBound::BigInt(*n)),
        Value::Int(n) => Some(PartitionBound::Int(*n)),
        Value::SmallInt(n) => Some(PartitionBound::SmallInt(*n)),
        Value::Date(d) => Some(PartitionBound::Date(*d)),
        Value::Text(s) => match crate::eval::parse_timestamp_literal(s) {
            Some(m) => Some(PartitionBound::TimestampTz(m)),
            None => Some(PartitionBound::Text(s.clone().into_owned())),
        },
        _ => None,
    }
}

fn bound_cmp(a: &PartitionBound, b: &PartitionBound) -> core::cmp::Ordering {
    use PartitionBound::{MaxValue, MinValue, Text};
    use core::cmp::Ordering;
    match (a, b) {
        (MinValue, MinValue) | (MaxValue, MaxValue) => Ordering::Equal,
        (MinValue, _) => Ordering::Less,
        (_, MinValue) => Ordering::Greater,
        (MaxValue, _) => Ordering::Greater,
        (_, MaxValue) => Ordering::Less,
        // v7.37 D.45 — TEXT bounds order lexicographically; every other
        // ordered variant (int-family / DATE / TIMESTAMPTZ) shares the i64
        // space via `bound_i64`.
        (Text(x), Text(y)) => x.cmp(y),
        _ => match (bound_i64(a), bound_i64(b)) {
            (Some(x), Some(y)) => x.cmp(&y),
            // Genuinely incomparable (e.g. TEXT vs numeric) — conservative
            // Equal keeps overlap detection from spuriously separating them.
            _ => Ordering::Equal,
        },
    }
}

/// Two half-open ranges `[a_lo, a_hi)` and `[b_lo, b_hi)` overlap iff
/// `a_lo < b_hi && b_lo < a_hi`. Used by the engine's child-create
/// path to reject siblings that would shadow each other.
pub(crate) fn ranges_overlap(
    a_lo: &PartitionBound,
    a_hi: &PartitionBound,
    b_lo: &PartitionBound,
    b_hi: &PartitionBound,
) -> bool {
    use core::cmp::Ordering;
    bound_cmp(a_lo, b_hi) == Ordering::Less && bound_cmp(b_lo, a_hi) == Ordering::Less
}

/// True when `value` falls inside the half-open `[lower, upper)`
/// range of a `Range` child. Caller passes the key's i64 micros
/// (extracted at INSERT time via `Value::Timestamptz`); the result
/// follows PG's "inclusive-lower / exclusive-upper" rule.
#[allow(dead_code)] // wired by commit #4 (INSERT routing)
/// v7.37 D.45 — a routed key (as a [`PartitionBound`] built by
/// [`value_to_bound`]) falls in `[lower, upper)`: PG's inclusive-lower /
/// exclusive-upper rule. Works for every ordered bound type (int-family /
/// DATE / TIMESTAMPTZ / TEXT), not just TIMESTAMPTZ; MINVALUE/MAXVALUE
/// sentinels are handled by `bound_cmp`.
pub(crate) fn value_in_range(
    key: &PartitionBound,
    lower: &PartitionBound,
    upper: &PartitionBound,
) -> bool {
    use core::cmp::Ordering;
    // key >= lower (inclusive) AND key < upper (exclusive).
    bound_cmp(key, lower) != Ordering::Less && bound_cmp(key, upper) == Ordering::Less
}

/// v7.37.16 (16.2) — PG-compatible-ish hash for a typed
/// [`Value`]. PG uses a per-type `hashfn` (e.g. `hashint4`,
/// `hashint8`, `hashtext`) that ultimately funnel through
/// `hash_any_extended` and produce a `uint64`. We don't yet
/// implement those exact opclass hashes — we use a stable
/// `fnv1a-64` over a type-tagged byte-canonicalisation of the
/// `Value` so:
///   1. the same value always lands in the same bucket within a
///      single SPG cluster (routing determinism), and
///   2. dump/restore round-trip stays exact (the bucket choice is
///      a pure function of catalog state + value, no `getpid()` /
///      `hash_seed` non-determinism).
///
/// We will swap this for PG's per-type `hashfn` family in
/// v7.37.16.13 when full PG hash compatibility (binary equality
/// with PG-issued `pg_dump --format=c` HASH partitions) becomes
/// load-bearing for a customer. For now this is "good enough
/// HASH routing" + a TODO bookmark.
#[allow(dead_code)] // wired by routing path; tests call via the public route.
pub(crate) fn pg_compatible_hash(value: &Value<'_>) -> u64 {
    // FNV-1a 64-bit (RFC reference constants — deterministic
    // across hosts + endianness).
    const FNV_OFFSET: u64 = 0xcbf29ce484222325;
    const FNV_PRIME: u64 = 0x100000001b3;
    let mut h: u64 = FNV_OFFSET;
    let mut feed = |bytes: &[u8]| {
        for &b in bytes {
            h ^= u64::from(b);
            h = h.wrapping_mul(FNV_PRIME);
        }
    };
    // Type-tag first so two values of different types never alias
    // even when their byte layout happens to coincide.
    match value {
        Value::Null => feed(&[0u8]),
        Value::Bool(b) => {
            feed(&[1u8]);
            feed(&[u8::from(*b)]);
        }
        Value::SmallInt(n) => {
            feed(&[2u8]);
            feed(&n.to_le_bytes());
        }
        Value::Int(n) => {
            feed(&[3u8]);
            feed(&n.to_le_bytes());
        }
        Value::BigInt(n) => {
            feed(&[4u8]);
            feed(&n.to_le_bytes());
        }
        Value::Float(f) => {
            feed(&[6u8]);
            feed(&f.to_bits().to_le_bytes());
        }
        Value::Text(s) => {
            feed(&[7u8]);
            feed(s.as_bytes());
        }
        Value::Date(d) => {
            feed(&[8u8]);
            feed(&d.to_le_bytes());
        }
        Value::Timestamp(m) => {
            feed(&[9u8]);
            feed(&m.to_le_bytes());
        }
        // Anything else (Bytea, Numeric, UUID, JSON, Vector,
        // Interval, Time, Array, …) folds through its Debug form
        // for now — same determinism guarantee, lower binary
        // compatibility with PG. v7.37.16.13 will replace per
        // type.
        other => {
            feed(&[255u8]);
            feed(format!("{other:?}").as_bytes());
        }
    }
    h
}

/// Render a [`PartitionBound`] for diagnostic / error messages.
pub(crate) fn bound_to_diag(b: &PartitionBound) -> String {
    match b {
        PartitionBound::MinValue => "MINVALUE".to_string(),
        PartitionBound::MaxValue => "MAXVALUE".to_string(),
        PartitionBound::TimestampTz(m) => format!("'{m}'::timestamptz"),
        // v7.37.16 (16.6) — diagnostic renderers for the extended
        // bound types. These appear in EXPLAIN + error messages
        // when describing LIST partitions or future Range-on-X.
        PartitionBound::BigInt(n) => format!("{n}::bigint"),
        PartitionBound::Int(n) => format!("{n}::integer"),
        PartitionBound::SmallInt(n) => format!("{n}::smallint"),
        PartitionBound::Date(d) => format!("{d}::date"),
        PartitionBound::Text(s) => format!("'{}'", s.replace('\'', "''")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use PartitionBound::{MaxValue, MinValue, TimestampTz};

    /// MINVALUE / MAXVALUE behave as ±∞ sentinels against finite
    /// TIMESTAMPTZ literals; equality-with-self stays Equal.
    #[test]
    fn bound_cmp_min_max_sentinels() {
        use core::cmp::Ordering;
        assert_eq!(bound_cmp(&MinValue, &MinValue), Ordering::Equal);
        assert_eq!(bound_cmp(&MaxValue, &MaxValue), Ordering::Equal);
        assert_eq!(bound_cmp(&MinValue, &TimestampTz(0)), Ordering::Less);
        assert_eq!(bound_cmp(&TimestampTz(0), &MaxValue), Ordering::Less);
        assert_eq!(
            bound_cmp(&TimestampTz(100), &TimestampTz(99)),
            Ordering::Greater
        );
    }

    /// `[a, b)` overlap rules — adjacent windows don't overlap; the
    /// upper bound is exclusive. Sentori partitions monthly windows
    /// `[2026-06-01, 2026-07-01)` + `[2026-07-01, 2026-08-01)`, so
    /// this is the load-bearing check for child registration.
    #[test]
    fn ranges_overlap_half_open() {
        let a_lo = TimestampTz(0);
        let a_hi = TimestampTz(100);
        let b_lo = TimestampTz(100);
        let b_hi = TimestampTz(200);
        let c_lo = TimestampTz(50);
        let c_hi = TimestampTz(150);
        // Adjacent: a ends where b starts — half-open ⇒ no overlap.
        assert!(!ranges_overlap(&a_lo, &a_hi, &b_lo, &b_hi));
        // Straddle: c overlaps a (50..100) and b (100..150).
        assert!(ranges_overlap(&a_lo, &a_hi, &c_lo, &c_hi));
        assert!(ranges_overlap(&b_lo, &b_hi, &c_lo, &c_hi));
        // Self-overlap (caller uses this as the empty-range guard).
        assert!(ranges_overlap(&a_lo, &a_hi, &a_lo, &a_hi));
        // Empty range (lower == upper): no self-overlap, so the
        // engine rejects it at child create time.
        assert!(!ranges_overlap(&a_lo, &a_lo, &a_lo, &a_lo));
        // MINVALUE / MAXVALUE: spans the whole timeline, overlaps
        // everything finite.
        assert!(ranges_overlap(&MinValue, &MaxValue, &a_lo, &a_hi));
    }

    /// `value_in_range` follows PG's inclusive-lower /
    /// exclusive-upper rule. Sentinel bounds skip the comparison.
    #[test]
    fn value_in_range_inclusive_lower_exclusive_upper() {
        use PartitionBound::{Date, Int};
        assert!(value_in_range(
            &TimestampTz(50),
            &TimestampTz(0),
            &TimestampTz(100)
        ));
        assert!(value_in_range(
            &TimestampTz(0),
            &TimestampTz(0),
            &TimestampTz(100)
        ));
        assert!(!value_in_range(
            &TimestampTz(100),
            &TimestampTz(0),
            &TimestampTz(100)
        ));
        assert!(!value_in_range(
            &TimestampTz(-1),
            &TimestampTz(0),
            &TimestampTz(100)
        ));
        // MINVALUE always satisfies lower; MAXVALUE always satisfies
        // upper (the catch-all "everything" range).
        assert!(value_in_range(&TimestampTz(i64::MIN), &MinValue, &MaxValue));
        assert!(value_in_range(
            &TimestampTz(i64::MAX - 1),
            &MinValue,
            &MaxValue
        ));
        // MAXVALUE as lower or MINVALUE as upper rejects everything.
        assert!(!value_in_range(&TimestampTz(0), &MaxValue, &MaxValue));
        assert!(!value_in_range(&TimestampTz(0), &MinValue, &MinValue));
        // v7.37 D.45 — INTEGER and DATE range keys route by their own type.
        assert!(value_in_range(&Int(5), &Int(0), &Int(10)));
        assert!(!value_in_range(&Int(10), &Int(0), &Int(10))); // upper exclusive
        assert!(value_in_range(&Int(0), &Int(0), &Int(10))); // lower inclusive
        assert!(value_in_range(&Int(12), &Int(10), &MaxValue));
        assert!(value_in_range(&Date(100), &Date(0), &Date(365)));
        assert!(!value_in_range(&Date(400), &Date(0), &Date(365)));
    }
}
