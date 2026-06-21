//! Per-query byte budget and heap-size estimators for join/filter
//! materialisation. Lifted out of `lib.rs` (v7.32 engine
//! modularisation). `ByteBudget` is the net-accounting meter the
//! v7.30.3 (mailrs round-26) bounded-join path charges/releases as
//! stages clone and free rows; the `approx_*` helpers estimate the
//! resident heap cost of a `Value` / `Row` / row set.

use alloc::string::String;

use spg_storage::{Row, Value};

use crate::EngineError;

/// v7.30.3 (mailrs round-26) — approximate heap bytes held by one
/// `Value`. Fat payloads (text / json / bytea / vectors / arrays)
/// dominate; fixed-size variants count 0 here because the per-cell
/// enum overhead is charged separately in `approx_row_bytes`. An
/// under-estimate is acceptable — the budget is a host-pressure
/// guard, not an exact meter.
pub(crate) fn approx_value_bytes(v: &Value) -> usize {
    match v {
        Value::Text(s) | Value::Json(s) => s.len(),
        Value::Bytes(b) => b.len(),
        Value::Vector(v) => v.len() * 4,
        Value::TextArray(a) => a
            .iter()
            .map(|o| o.as_ref().map_or(0, String::len) + 8)
            .sum(),
        Value::IntArray(a) => a.len() * 8,
        _ => 0,
    }
}

/// Approximate heap bytes held by one materialised `Row`: per-cell
/// enum slots plus fat payloads.
pub(crate) fn approx_row_bytes(row: &Row<'static>) -> usize {
    row.values.len() * core::mem::size_of::<Value>()
        + row.values.iter().map(approx_value_bytes).sum::<usize>()
}

/// v7.30.3 (mailrs round-26) — per-query byte budget for join/filter
/// materialisation. Net accounting: stages charge what they clone and
/// release what they free (`working` is released when the next stage
/// replaces it), so the meter tracks live bytes, not cumulative
/// churn. `limit = usize::MAX` when the budget is disabled keeps the
/// hot path branch-free apart from one saturating add + compare.
pub(crate) struct ByteBudget {
    limit: usize,
    used: usize,
}

impl ByteBudget {
    pub(crate) const fn new(limit: Option<usize>) -> Self {
        Self {
            limit: match limit {
                Some(n) => n,
                None => usize::MAX,
            },
            used: 0,
        }
    }

    pub(crate) fn charge(&mut self, n: usize) -> Result<(), EngineError> {
        self.used = self.used.saturating_add(n);
        if self.used > self.limit {
            return Err(EngineError::QueryBytesExceeded(self.limit));
        }
        Ok(())
    }

    pub(crate) fn release(&mut self, n: usize) {
        self.used = self.used.saturating_sub(n);
    }
}

/// Sum `approx_row_bytes` over a freshly materialised row set.
pub(crate) fn approx_rows_bytes(rows: &[Row<'static>]) -> usize {
    rows.iter().map(approx_row_bytes).sum()
}
