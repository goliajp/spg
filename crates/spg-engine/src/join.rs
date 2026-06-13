//! Join execution data structures — the deferred-join row sources
//! (JoinSrc), the row-index-tuple view handed to the aggregate engine
//! (RowRef), the per-stage peer descriptor (JoinedPeer), the deferred
//! output (DeferredJoin), and the tuple<->Row helpers. Split out of
//! `lib.rs` (v7.32 engine modularisation). The join planner methods on
//! Engine that build these still live in lib.rs (and use this module).

use alloc::borrow::Cow;
use alloc::string::String;
use alloc::vec::Vec;

use spg_sql::ast::{Expr, JoinKind, SelectStatement};
use spg_storage::{ColumnSchema, Row, Value};

use crate::approx_value_bytes;

/// v7.17.0 Phase 3.P0-41 — LATERAL peer descriptor. Either eagerly
/// materialised (every regular table / unnest / generate_series) or
/// lateral (subquery re-evaluated per outer row).
pub(crate) struct JoinedPeer<'a> {
    pub(crate) eager_rows: Option<Vec<Row>>,
    pub(crate) cols: Vec<ColumnSchema>,
    pub(crate) alias: String,
    pub(crate) kind: JoinKind,
    pub(crate) on: Option<&'a Expr>,
    pub(crate) lateral: Option<&'a SelectStatement>,
    /// v7.28 (round-22) — plain-table name for the index-nested-loop
    /// path. None for unnest/lateral.
    pub(crate) join_table: Option<String>,
}

/// v7.31 (perf campaign) — deferred-join row source: one per join
/// stage. The working set advances as row-index tuples instead of
/// cloned combined rows; each tuple slot indexes into one of these.
pub(crate) enum JoinSrc<'a> {
    /// Owned by the join: the primary scan, a lazily-materialised
    /// peer, or the arena of per-outer-row LATERAL results.
    Owned(Vec<Row>),
    /// Peer rows materialised up front and still owned by `JoinedPeer`.
    Eager(&'a [Row]),
    /// Index-nested-loop peer reading the stored table in place.
    Stored(&'a spg_storage::persistent::PersistentVec<Row>),
}

impl JoinSrc<'_> {
    pub(crate) fn get(&self, i: usize) -> Option<&Row> {
        match self {
            Self::Owned(v) => v.get(i),
            Self::Eager(s) => s.get(i),
            Self::Stored(p) => p.get(i),
        }
    }

    pub(crate) fn len(&self) -> usize {
        match self {
            Self::Owned(v) => v.len(),
            Self::Eager(s) => s.len(),
            Self::Stored(p) => p.len(),
        }
    }
}

/// Resolve one combined-schema position against a row-index tuple.
/// `offsets` holds the prefix column offsets of the consumed sources
/// (`offsets.len() == tuple.len() + 1`). `None` means SQL NULL: a
/// LEFT-extended slot (`usize::MAX`), or a position past the row's
/// width.
pub(crate) fn tuple_value<'s>(
    sources: &'s [JoinSrc<'_>],
    offsets: &[usize],
    tuple: &[usize],
    pos: usize,
) -> Option<&'s Value> {
    let k = offsets.partition_point(|&o| o <= pos).checked_sub(1)?;
    let ri = *tuple.get(k)?;
    if ri == usize::MAX {
        return None;
    }
    sources.get(k)?.get(ri)?.values.get(pos - offsets[k])
}

/// v7.32 (P4 borrow channel, increment 2) — a row handed to the
/// aggregate engine. Either a borrowed materialised `Row` (single-table
/// and legacy paths) or a deferred row-index tuple over join sources
/// (the join+aggregate path) that resolves cells *by reference* via
/// `tuple_value`, so the join+aggregate path never materialises a
/// combined `Row` for the bound-column fast path.
pub(crate) enum RowRef<'a> {
    Owned(&'a Row),
    Tuple {
        sources: &'a [JoinSrc<'a>],
        offsets: &'a [usize],
        tuple: &'a [usize],
    },
}

impl RowRef<'_> {
    /// Borrow the cell at a combined-schema position. The bound-column
    /// fast path in `aggregate::run` reads cells this way — zero clone.
    #[inline]
    pub(crate) fn get(&self, pos: usize) -> Option<&Value> {
        match self {
            RowRef::Owned(r) => r.values.get(pos),
            RowRef::Tuple {
                sources,
                offsets,
                tuple,
            } => tuple_value(sources, offsets, tuple, pos),
        }
    }

    /// Present the row as a `&Row` for the eval path. `Owned` borrows
    /// directly (zero cost); `Tuple` materialises once into owned values
    /// — the only allocation, paid solely on the eval (non-bound) path,
    /// never for the bound fast path. The materialised width is the full
    /// combined schema (`offsets.last()`); a LEFT-NULL slot or an out-of-
    /// range position becomes `Value::Null` (same as `tuple_value`).
    pub(crate) fn as_row(&self) -> Cow<'_, Row> {
        match self {
            RowRef::Owned(r) => Cow::Borrowed(r),
            RowRef::Tuple {
                sources,
                offsets,
                tuple,
            } => {
                let width = offsets.last().copied().unwrap_or(0);
                let mut vals: Vec<Value> = Vec::with_capacity(width);
                for pos in 0..width {
                    vals.push(
                        tuple_value(sources, offsets, tuple, pos)
                            .cloned()
                            .unwrap_or(Value::Null),
                    );
                }
                Cow::Owned(Row::new(vals))
            }
        }
    }
}

/// Clone a source row's values into a combined-row buffer. A mask
/// (per-column "is referenced anywhere in the statement") NULLs the
/// unreferenced columns instead of cloning them — the in-place
/// equivalent of `null_out_unreferenced` for sources that were never
/// pre-cloned.
pub(crate) fn extend_masked(vals: &mut Vec<Value>, row: &Row, mask: Option<&[bool]>) {
    match mask {
        Some(keep) => {
            for (i, v) in row.values.iter().enumerate() {
                if keep.get(i).copied().unwrap_or(false) {
                    vals.push(v.clone());
                } else {
                    vals.push(Value::Null);
                }
            }
        }
        None => vals.extend(row.values.iter().cloned()),
    }
}

/// Materialise a row-index tuple into owned values, NULL-padding
/// LEFT-extended slots to the source's schema width.
pub(crate) fn materialise_tuple_vals(
    sources: &[JoinSrc<'_>],
    widths: &[usize],
    masks: &[Option<Vec<bool>>],
    tuple: &[usize],
    cap: usize,
) -> Vec<Value> {
    let mut vals: Vec<Value> = Vec::with_capacity(cap);
    for (k, &ri) in tuple.iter().enumerate() {
        let row = if ri == usize::MAX {
            None
        } else {
            sources[k].get(ri)
        };
        match row {
            Some(r) => extend_masked(&mut vals, r, masks[k].as_deref()),
            None => {
                for _ in 0..widths[k] {
                    vals.push(Value::Null);
                }
            }
        }
    }
    vals
}

/// v7.32 (P4 borrow channel, increment 2) — the deferred output of
/// `build_joined_filtered_rows`: WHERE-surviving rows held as row-index
/// tuples over the join sources, NOT materialised into combined Rows.
/// The aggregate path borrows each survivor as a `RowRef::Tuple` (the
/// bound fast path reads source cells by reference — zero clone); the
/// projection / window paths call `materialise()` for an owned
/// `Vec<Row>` identical to the pre-increment-2 output.
pub(crate) struct DeferredJoin<'a> {
    pub(crate) sources: Vec<JoinSrc<'a>>,
    pub(crate) offsets: Vec<usize>,
    pub(crate) widths: Vec<usize>,
    pub(crate) masks: Vec<Option<Vec<bool>>>,
    /// Flat row-index tuples — one stride-long group per surviving row.
    pub(crate) survivors: Vec<usize>,
    pub(crate) stride: usize,
    pub(crate) combined_schema: Vec<ColumnSchema>,
}

impl DeferredJoin<'_> {
    pub(crate) fn len(&self) -> usize {
        if self.stride == 0 {
            0
        } else {
            self.survivors.len() / self.stride
        }
    }

    /// Borrow each surviving tuple as a `RowRef::Tuple` for the
    /// aggregate engine — no combined Row is materialised.
    pub(crate) fn row_refs(&self) -> Vec<RowRef<'_>> {
        if self.stride == 0 {
            return Vec::new();
        }
        self.survivors
            .chunks(self.stride)
            .map(|tuple| RowRef::Tuple {
                sources: &self.sources,
                offsets: &self.offsets,
                tuple,
            })
            .collect()
    }

    /// Materialise the survivors into owned combined Rows (projection /
    /// window paths). Byte-identical to the pre-deferral output.
    pub(crate) fn materialise(&self) -> Vec<Row> {
        if self.stride == 0 {
            return Vec::new();
        }
        let cap = self.offsets.last().copied().unwrap_or(0);
        self.survivors
            .chunks(self.stride)
            .map(|tuple| {
                Row::new(materialise_tuple_vals(
                    &self.sources,
                    &self.widths,
                    &self.masks,
                    tuple,
                    cap,
                ))
            })
            .collect()
    }
}

/// v7.32 (P4 borrow channel, increment 2) — byte estimate of a
/// row-index tuple WITHOUT materialising it: walk each referenced source
/// cell by reference and sum, applying the same per-column mask
/// `materialise_tuple_vals` would (unreferenced columns count as NULL).
/// Mirrors `approx_row_bytes(materialised)` so the v7.30.3 byte budget
/// meters identical live bytes on the deferred path.
pub(crate) fn approx_tuple_bytes(
    sources: &[JoinSrc<'_>],
    offsets: &[usize],
    masks: &[Option<Vec<bool>>],
    tuple: &[usize],
) -> usize {
    let width = offsets.last().copied().unwrap_or(0);
    let mut bytes = width * core::mem::size_of::<Value>();
    for (k, &ri) in tuple.iter().enumerate() {
        if ri == usize::MAX {
            continue;
        }
        let Some(row) = sources.get(k).and_then(|s| s.get(ri)) else {
            continue;
        };
        let mask = masks.get(k).and_then(|m| m.as_deref());
        for (i, v) in row.values.iter().enumerate() {
            let kept = mask.map_or(true, |m| m.get(i).copied().unwrap_or(false));
            if kept {
                bytes += approx_value_bytes(v);
            }
        }
    }
    bytes
}
