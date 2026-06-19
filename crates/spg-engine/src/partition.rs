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
                if parent_name == parent =>
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
        Value::Date(days) => {
            // PG-style: a DATE as a TIMESTAMPTZ bound expands to
            // midnight UTC of that day, in microseconds.
            let micros = i64::from(days) * 86_400i64 * 1_000_000i64;
            Ok(PartitionBound::TimestampTz(micros))
        }
        Value::Text(s) => {
            // Text literal that didn't fold through the parser's typed
            // path(common after a placeholder substitution). Reuse
            // the same TIMESTAMPTZ parser the regular Value path uses;
            // surface a parser miss as Unsupported.
            match crate::eval::parse_timestamp_literal(&s) {
                Some(micros) => Ok(PartitionBound::TimestampTz(micros)),
                None => Err(EngineError::Unsupported(format!(
                    "PARTITION OF: bound literal {s:?} not recognised \
                     as a TIMESTAMPTZ"
                ))),
            }
        }
        other => Err(EngineError::Unsupported(format!(
            "PARTITION OF: bound must be TIMESTAMPTZ literal or \
             MINVALUE/MAXVALUE, got {other:?}"
        ))),
    }
}

/// Range bound order, used by overlap detection and INSERT routing.
/// `MinValue` is below every TIMESTAMPTZ; `MaxValue` is above every
/// TIMESTAMPTZ. Two `TimestampTz` micros compare as `i64`.
fn bound_cmp(a: &PartitionBound, b: &PartitionBound) -> core::cmp::Ordering {
    use PartitionBound::{MaxValue, MinValue, TimestampTz};
    use core::cmp::Ordering;
    match (a, b) {
        (MinValue, MinValue) | (MaxValue, MaxValue) => Ordering::Equal,
        (MinValue, _) => Ordering::Less,
        (_, MinValue) => Ordering::Greater,
        (MaxValue, _) => Ordering::Greater,
        (_, MaxValue) => Ordering::Less,
        (TimestampTz(x), TimestampTz(y)) => x.cmp(y),
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
pub(crate) fn value_in_range(
    value_micros: i64,
    lower: &PartitionBound,
    upper: &PartitionBound,
) -> bool {
    let lower_ok = match lower {
        PartitionBound::MinValue => true,
        PartitionBound::MaxValue => false,
        PartitionBound::TimestampTz(m) => value_micros >= *m,
    };
    let upper_ok = match upper {
        PartitionBound::MinValue => false,
        PartitionBound::MaxValue => true,
        PartitionBound::TimestampTz(m) => value_micros < *m,
    };
    lower_ok && upper_ok
}

/// Render a [`PartitionBound`] for diagnostic / error messages.
pub(crate) fn bound_to_diag(b: &PartitionBound) -> String {
    match b {
        PartitionBound::MinValue => "MINVALUE".to_string(),
        PartitionBound::MaxValue => "MAXVALUE".to_string(),
        PartitionBound::TimestampTz(m) => format!("'{m}'::timestamptz"),
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
        assert!(value_in_range(50, &TimestampTz(0), &TimestampTz(100)));
        assert!(value_in_range(0, &TimestampTz(0), &TimestampTz(100)));
        assert!(!value_in_range(100, &TimestampTz(0), &TimestampTz(100)));
        assert!(!value_in_range(-1, &TimestampTz(0), &TimestampTz(100)));
        // MINVALUE always satisfies lower; MAXVALUE always satisfies
        // upper (the catch-all "everything" range).
        assert!(value_in_range(i64::MIN, &MinValue, &MaxValue));
        assert!(value_in_range(i64::MAX - 1, &MinValue, &MaxValue));
        // MAXVALUE as lower or MINVALUE as upper rejects everything.
        assert!(!value_in_range(0, &MaxValue, &MaxValue));
        assert!(!value_in_range(0, &MinValue, &MinValue));
    }
}
