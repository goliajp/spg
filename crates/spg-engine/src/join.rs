//! Join execution — the deferred-join row sources (JoinSrc), the
//! row-index-tuple view handed to the aggregate engine (RowRef), the
//! per-stage peer descriptor (JoinedPeer), the deferred output
//! (DeferredJoin), the bounded top-N sink (TopNEntry), the tuple<->Row
//! helpers, and the `Engine` join planner methods that build them
//! (`build_joined_filtered_rows`, the LATERAL probe/materialise pair,
//! and the streamed inner-join top-N path). Split out of `lib.rs`
//! (v7.32 engine modularisation).

use alloc::borrow::Cow;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use spg_sql::ast::{Expr, FromClause, JoinKind, SelectItem, SelectStatement, TableRef};
use spg_storage::{ColumnSchema, DataType, Row, Table, Value};

use crate::eval::EvalContext;
use crate::{
    ByteBudget, CancelToken, Engine, EngineError, OrderKey, QueryResult, aggregate,
    apply_offset_and_limit, approx_row_bytes, approx_rows_bytes, approx_value_bytes,
    build_order_keys, build_projection, cmp_multi_key, collect_column_qualifiers,
    collect_qualified_refs, eval, expr_has_subquery, memoize, reorder, value_cmp,
    value_to_literal_expr,
};

/// v7.17.0 Phase 3.P0-41 — LATERAL peer descriptor. Either eagerly
/// materialised (every regular table / unnest / generate_series) or
/// lateral (subquery re-evaluated per outer row).
pub(crate) struct JoinedPeer<'a> {
    pub(crate) eager_rows: Option<Vec<Row<'static>>>,
    pub(crate) cols: Vec<ColumnSchema>,
    pub(crate) alias: String,
    pub(crate) kind: JoinKind,
    pub(crate) on: Option<&'a Expr>,
    pub(crate) lateral: Option<&'a SelectStatement>,
    /// v7.28 (round-22) — plain-table name for the index-nested-loop
    /// path. None for unnest/lateral.
    pub(crate) join_table: Option<String>,
    /// v7.33 (mailrs 7.33.0) — WHERE conjuncts pushed onto this (INNER)
    /// peer that were NOT applied by eager materialisation. A deferred
    /// plain peer carries them here so the join stages apply them as a
    /// residual filter on matched (left,right) pairs — keeping the
    /// index-nested-loop path (seek driver + look up only matched peer
    /// rows) instead of eagerly scanning the whole peer table to filter
    /// it. Empty for eager peers (already filtered) and LEFT peers
    /// (analyze_join_pushdown only pushes onto INNER peers).
    pub(crate) where_preds: Vec<Expr>,
}

/// v7.31 (perf campaign) — deferred-join row source: one per join
/// stage. The working set advances as row-index tuples instead of
/// cloned combined rows; each tuple slot indexes into one of these.
pub(crate) enum JoinSrc<'a> {
    /// Owned by the join: the primary scan, a lazily-materialised
    /// peer, or the arena of per-outer-row LATERAL results.
    Owned(Vec<Row<'static>>),
    /// Peer rows materialised up front and still owned by `JoinedPeer`.
    Eager(&'a [Row<'static>]),
    /// Index-nested-loop peer reading the stored table in place.
    Stored(&'a spg_storage::persistent::PersistentVec<Row<'static>>),
    /// v7.36 — hot tier borrowed in place + cold tier owned. INL
    /// probe consults `cold_locator_map` to translate a Cold
    /// `RowLocator` (which only carries `(segment_id, page_offset)`
    /// — that pair identifies the PAGE, not the row, so multiple
    /// rows on one page collide) into a per-row offset via the
    /// PK key (`IndexKey::Int(i64)`) instead. The cold-tier
    /// architecture already requires an integer PK, so this is
    /// the unique-per-row identifier the segment lookup already
    /// uses internally. Indices `0..hot.len()` map to hot rows;
    /// `hot.len()..` map to `cold[i - hot.len()]`.
    Mixed {
        hot: &'a spg_storage::persistent::PersistentVec<Row<'static>>,
        cold: Vec<Row<'static>>,
        cold_locator_map: hashbrown::HashMap<i64, usize>,
    },
}

/// v7.39 (round 576) — a hash-join bucket that does not allocate for the
/// row it usually holds.
///
/// The build side of an FK-to-PK join is unique, so nearly every bucket
/// holds exactly one row — and each one was its own `Vec`. A counting
/// allocator put the cost at ONE allocation per peer row: a 200k-row
/// self-join made 200,127 allocations where a single-table scan of the
/// same table makes 47, and it made them whether the query wanted
/// 200,000 rows or 100. That is the allocator's 28% round 575 could not
/// name, and the reason the join does not get cheaper when a predicate
/// narrows it.
///
/// The first row lives inline; a second one promotes to a `Vec`.
#[derive(Debug)]
enum Bucket {
    One(usize),
    Many(Vec<usize>),
}

impl Bucket {
    fn push(&mut self, ri: usize) {
        match self {
            Self::One(first) => *self = Self::Many(alloc::vec![*first, ri]),
            Self::Many(v) => v.push(ri),
        }
    }

    fn as_slice(&self) -> &[usize] {
        match self {
            Self::One(x) => core::slice::from_ref(x),
            Self::Many(v) => v.as_slice(),
        }
    }
}

impl JoinSrc<'_> {
    pub(crate) fn get(&self, i: usize) -> Option<&Row<'static>> {
        match self {
            Self::Owned(v) => v.get(i),
            Self::Eager(s) => s.get(i),
            Self::Stored(p) => p.get(i),
            Self::Mixed { hot, cold, .. } => {
                if i < hot.len() {
                    hot.get(i)
                } else {
                    cold.get(i - hot.len())
                }
            }
        }
    }

    pub(crate) fn len(&self) -> usize {
        match self {
            Self::Owned(v) => v.len(),
            Self::Eager(s) => s.len(),
            Self::Stored(p) => p.len(),
            Self::Mixed { hot, cold, .. } => hot.len() + cold.len(),
        }
    }

    /// v7.36 — translate a PK key (`i64` — the cold tier's
    /// integer-only PK contract) into the corresponding row index
    /// inside this `Mixed` source. Returns `None` for non-Mixed
    /// sources or when the key has no cold-tier row registered.
    pub(crate) fn cold_pk_offset(&self, pk_key: i64) -> Option<usize> {
        match self {
            Self::Mixed {
                hot,
                cold_locator_map,
                ..
            } => cold_locator_map
                .get(&pk_key)
                .copied()
                .map(|off| hot.len() + off),
            _ => None,
        }
    }
}

/// Resolve one combined-schema position against a row-index tuple.
/// `offsets` holds the prefix column offsets of the consumed sources
/// (`offsets.len() == tuple.len() + 1`). `None` means SQL NULL: a
/// LEFT-extended slot (`usize::MAX`), or a position past the row's
/// width.
///
/// v7.37.43 (DISTA A-2) — slow-path fallback used only when the caller
/// has no `pos_to_src` table. Most RowRef::Tuple uses now go through
/// `tuple_value_indexed` (direct index lookup, no partition_point).
pub(crate) fn tuple_value<'s>(
    sources: &'s [JoinSrc<'_>],
    offsets: &[usize],
    tuple: &[usize],
    pos: usize,
) -> Option<&'s Value<'static>> {
    let k = offsets.partition_point(|&o| o <= pos).checked_sub(1)?;
    let ri = *tuple.get(k)?;
    if ri == usize::MAX {
        return None;
    }
    sources.get(k)?.get(ri)?.values.get(pos - offsets[k])
}

/// v7.37.43 (DISTA A-2) — direct-index variant: `pos_to_src[pos]` =
/// source index `k` for combined position `pos`. Built once per
/// JoinPipeline / DeferredJoin (linear in combined width); per-row
/// `RowRef::get` becomes a single array read instead of a binary
/// search over `offsets` per call.
///
/// For DISTA (~100k joined rows × ~5 cell reads/row in the aggregate
/// loop) this strips ~50 ns × 500k = ~25 ms of partition_point ops down
/// to direct indexing.
#[inline]
pub(crate) fn tuple_value_indexed<'s>(
    sources: &'s [JoinSrc<'_>],
    offsets: &[usize],
    pos_to_src: &[u16],
    tuple: &[usize],
    pos: usize,
) -> Option<&'s Value<'static>> {
    let k = *pos_to_src.get(pos)? as usize;
    let ri = *tuple.get(k)?;
    if ri == usize::MAX {
        return None;
    }
    sources.get(k)?.get(ri)?.values.get(pos - offsets[k])
}

/// v7.37.43 (DISTA A-2) — build the position → source-index table for
/// a combined schema. `offsets.len() == sources + 1`, last entry is
/// the total combined width.
pub(crate) fn build_pos_to_src(offsets: &[usize]) -> Vec<u16> {
    let width = offsets.last().copied().unwrap_or(0);
    let mut tab: Vec<u16> = Vec::with_capacity(width);
    for k in 0..offsets.len().saturating_sub(1) {
        let span = offsets[k + 1] - offsets[k];
        for _ in 0..span {
            // 2^16 sources is comfortably beyond the planner cap;
            // `as u16` truncation here is a non-issue in practice.
            tab.push(k as u16);
        }
    }
    tab
}

/// v7.32 (P4 borrow channel, increment 2) — a row handed to the
/// aggregate engine. Either a borrowed materialised `Row` (single-table
/// and legacy paths) or a deferred row-index tuple over join sources
/// (the join+aggregate path) that resolves cells *by reference* via
/// `tuple_value`, so the join+aggregate path never materialises a
/// combined `Row` for the bound-column fast path.
pub(crate) enum RowRef<'a> {
    Owned(&'a Row<'static>),
    Tuple {
        sources: &'a [JoinSrc<'a>],
        offsets: &'a [usize],
        /// v7.37.43 (DISTA A-2) — precomputed combined-position →
        /// source-index map (built once per JoinPipeline / DeferredJoin
        /// in `build_pos_to_src`). `RowRef::get` uses this for direct
        /// indexing instead of binary search over `offsets` per call.
        pos_to_src: &'a [u16],
        tuple: &'a [usize],
    },
}

impl RowRef<'_> {
    /// Borrow the cell at a combined-schema position. The bound-column
    /// fast path in `aggregate::run` reads cells this way — zero clone.
    #[inline]
    pub(crate) fn get(&self, pos: usize) -> Option<&Value<'_>> {
        match self {
            RowRef::Owned(r) => r.values.get(pos),
            RowRef::Tuple {
                sources,
                offsets,
                pos_to_src,
                tuple,
            } => tuple_value_indexed(sources, offsets, pos_to_src, tuple, pos),
        }
    }

    /// Present the row as a `&Row<'static>` for the eval path. `Owned` borrows
    /// directly (zero cost); `Tuple` materialises once into owned values
    /// — the only allocation, paid solely on the eval (non-bound) path,
    /// never for the bound fast path. The materialised width is the full
    /// combined schema (`offsets.last()`); a LEFT-NULL slot or an out-of-
    /// range position becomes `Value::Null` (same as `tuple_value`).
    pub(crate) fn as_row(&self) -> Cow<'_, Row<'static>> {
        match self {
            RowRef::Owned(r) => Cow::Borrowed(r),
            RowRef::Tuple {
                sources,
                offsets,
                pos_to_src,
                tuple,
            } => {
                let width = offsets.last().copied().unwrap_or(0);
                let mut vals: Vec<Value<'static>> = Vec::with_capacity(width);
                for pos in 0..width {
                    vals.push(
                        tuple_value_indexed(sources, offsets, pos_to_src, tuple, pos)
                            .cloned()
                            .unwrap_or(Value::Null),
                    );
                }
                Cow::Owned(Row::new(vals))
            }
        }
    }

    /// v7.37.5-A2b (profile-guided Track A) — same as `as_row` but
    /// writes into a caller-owned buffer that survives the row loop.
    /// Profile showed `as_row` allocating + freeing a fresh
    /// `Vec<Value>` of full combined width per outer row in
    /// `accumulate_groups`'s `needs_mat` path (~15 % self time in
    /// `to_vec`/`Value::clone`/`drop` combined, ~10-20 MB per-query
    /// allocator churn at 24 k × 30-cell width). Reusing the buffer
    /// keeps the Vec backing across iterations — Value clones still
    /// fire (they're semantically owned) but the Vec allocation +
    /// free goes away. `Owned` rows clone into the buffer too so the
    /// caller can pass a single buffer through both branches; the
    /// Vec stays warm in the allocator across all calls.
    pub(crate) fn as_row_into(&self, buf: &mut Vec<Value<'static>>) {
        buf.clear();
        match self {
            RowRef::Owned(r) => {
                buf.reserve(r.values.len());
                for v in &r.values {
                    buf.push(v.clone());
                }
            }
            RowRef::Tuple {
                sources,
                offsets,
                pos_to_src,
                tuple,
            } => {
                let width = offsets.last().copied().unwrap_or(0);
                buf.reserve(width);
                for pos in 0..width {
                    buf.push(
                        tuple_value_indexed(sources, offsets, pos_to_src, tuple, pos)
                            .cloned()
                            .unwrap_or(Value::Null),
                    );
                }
            }
        }
    }
}

/// Clone a source row's values into a combined-row buffer. A mask
/// (per-column "is referenced anywhere in the statement") NULLs the
/// unreferenced columns instead of cloning them — the in-place
/// equivalent of `null_out_unreferenced` for sources that were never
/// pre-cloned.
pub(crate) fn extend_masked(
    vals: &mut Vec<Value<'static>>,
    row: &Row<'static>,
    mask: Option<&[bool]>,
) {
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
) -> Vec<Value<'static>> {
    let mut vals: Vec<Value<'static>> = Vec::with_capacity(cap);
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
/// `Vec<Row<'static>>` identical to the pre-increment-2 output.
pub(crate) struct DeferredJoin<'a> {
    pub(crate) sources: Vec<JoinSrc<'a>>,
    pub(crate) offsets: Vec<usize>,
    /// v7.37.43 (DISTA A-2) — combined-position → source-index map; built
    /// once via `build_pos_to_src(&offsets)` at construction time, so
    /// per-row `RowRef::get` is a direct index instead of a partition_point
    /// over `offsets`.
    pub(crate) pos_to_src: Vec<u16>,
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
                pos_to_src: &self.pos_to_src,
                tuple,
            })
            .collect()
    }

    /// Materialise the survivors into owned combined Rows (projection /
    /// window paths). Byte-identical to the pre-deferral output.
    pub(crate) fn materialise(&self) -> Vec<Row<'static>> {
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

/// v7.30.3 (mailrs round-26) — bounded top-N sink entry for the
/// streamed single-join path. `keys` are the `OrderKey`s
/// `build_order_keys` emits; `descs` (shared across all entries via
/// `Rc`) drives the per-key reverse so ordering matches the general
/// path's `cmp_multi_key` exactly (including the ±INF NULL placements
/// and full-precision text keys). `seq` is production order: ties keep
/// the earliest-produced rows, matching what the general path's stable
/// in-budget sort yields. The `BinaryHeap` is a max-heap, so `peek()`
/// is the worst kept row.
///
/// v7.37.16 — `keys` moved from `Vec<f64>` (DESC pre-encoded by
/// negation) to `Vec<OrderKey>`: text keys can't be negated, so DESC
/// is now applied by `cmp_multi_key` via the carried `descs`.
struct TopNEntry {
    keys: Vec<OrderKey>,
    descs: alloc::rc::Rc<[bool]>,
    seq: u64,
    row: Row<'static>,
}

impl PartialEq for TopNEntry {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == core::cmp::Ordering::Equal
    }
}
impl Eq for TopNEntry {}
impl PartialOrd for TopNEntry {
    fn partial_cmp(&self, other: &Self) -> Option<core::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for TopNEntry {
    fn cmp(&self, other: &Self) -> core::cmp::Ordering {
        cmp_multi_key(&self.keys, &other.keys, &self.descs).then(self.seq.cmp(&other.seq))
    }
}

// v7.28 (round-22) - intermediate-row ceiling: a join whose working set
// explodes errors instead of eating the host (mailrs watched RSS climb
// to 7 GiB of 15 before a manual restart). The ceiling is per join
// STAGE, not per query.
const MAX_JOIN_INTERMEDIATE_ROWS: usize = 4_000_000;

/// v7.32 — the accumulating state of the deferred-join pipeline: one
/// `JoinSrc` / mask / width per source joined so far, the prefix column
/// `offsets`, and the flat row-index tuple `working` set (`stride` =
/// sources joined, `usize::MAX` = a LEFT-join NULL slot). Each join
/// stage reads the prior state to probe the next peer and `advance`s the
/// pipeline by one source. `consumed_cols` tracks the combined-row width
/// built so far (the outer-left schema slice each lateral peer sees).
struct JoinPipeline<'a> {
    sources: Vec<JoinSrc<'a>>,
    masks: Vec<Option<Vec<bool>>>,
    widths: Vec<usize>,
    offsets: Vec<usize>,
    /// v7.37.43 (DISTA A-2) — combined-position → source-index map; kept
    /// in sync with `offsets` by `new` / `advance`.
    pos_to_src: Vec<u16>,
    working: Vec<usize>,
    stride: usize,
    consumed_cols: usize,
}

impl<'a> JoinPipeline<'a> {
    /// Seed the pipeline with the primary source (one stage, stride 1).
    fn new(
        primary: JoinSrc<'a>,
        mask: Option<Vec<bool>>,
        width: usize,
        working: Vec<usize>,
    ) -> Self {
        let offsets = alloc::vec![0, width];
        let pos_to_src = build_pos_to_src(&offsets);
        Self {
            sources: alloc::vec![primary],
            masks: alloc::vec![mask],
            widths: alloc::vec![width],
            offsets,
            pos_to_src,
            working,
            stride: 1,
            consumed_cols: width,
        }
    }

    /// Working-set row count (tuples / stride).
    fn rows(&self) -> usize {
        self.working.len() / self.stride
    }

    /// Consume one peer: replace the working set with `next`, append the
    /// peer's `source` / `mask` / width, and grow the stride + offsets.
    fn advance(
        &mut self,
        next: Vec<usize>,
        source: JoinSrc<'a>,
        mask: Option<Vec<bool>>,
        right_arity: usize,
    ) {
        self.working = next;
        self.stride += 1;
        self.sources.push(source);
        self.masks.push(mask);
        self.consumed_cols += right_arity;
        self.offsets.push(self.consumed_cols);
        self.widths.push(right_arity);
        // v7.37.43 (DISTA A-2) — extend the pos_to_src table for the
        // new peer's column span. `as u16` truncation is safe: source
        // counts in practice are O(small).
        let k = (self.sources.len() - 1) as u16;
        for _ in 0..right_arity {
            self.pos_to_src.push(k);
        }
    }
}

/// Per-source column mask: which columns the statement references
/// (`None` = keep all). In-place join sources apply it at
/// materialisation time instead of `null_out_unreferenced`.
fn keep_mask(
    needed: Option<&alloc::collections::BTreeSet<(String, String)>>,
    cols: &[ColumnSchema],
    alias: &str,
) -> Option<Vec<bool>> {
    let needed = needed?;
    let keep: Vec<bool> = cols
        .iter()
        .map(|c| needed.contains(&(alias.to_string(), c.name.clone())))
        .collect();
    if keep.iter().all(|k| *k) {
        None
    } else {
        Some(keep)
    }
}

/// Split a peer's ON into hash-join `eq_pairs` — `(left combined
/// position, right peer position)` — and the `residual` conjuncts that
/// evaluate on matched candidates. Both empty for a LATERAL peer or a
/// peer with no ON. The returned residual refs borrow the underlying ON
/// expressions (not the `peer` itself, since `peer.on` is a `Copy`
/// reference), so the caller can still mutate `peer` afterwards.
fn extract_join_keys<'a>(
    peer: &JoinedPeer<'a>,
    combined_schema: &[ColumnSchema],
    consumed_cols: usize,
) -> (Vec<(usize, usize)>, Vec<&'a Expr>) {
    let mut eq_pairs: Vec<(usize, usize)> = Vec::new();
    let mut residual: Vec<&Expr> = Vec::new();
    if let (Some(on_expr), None) = (peer.on, peer.lateral) {
        for sub in reorder::split_and_conjunctions(on_expr) {
            match match_equi_pair(sub, peer, combined_schema, consumed_cols) {
                Some(pair) => eq_pairs.push(pair),
                None => residual.push(sub),
            }
        }
    }
    (eq_pairs, residual)
}

/// One conjunct as an equi-join key for `peer`: `<left>.<col> = <peer>.<col>`
/// in either order, where the left side resolves inside the part of the
/// combined row already joined. `None` when it is anything else.
fn match_equi_pair(
    sub: &Expr,
    peer: &JoinedPeer<'_>,
    combined_schema: &[ColumnSchema],
    consumed_cols: usize,
) -> Option<(usize, usize)> {
    let Expr::Binary {
        lhs,
        op: spg_sql::ast::BinOp::Eq,
        rhs,
    } = sub
    else {
        return None;
    };
    let (Expr::Column(a), Expr::Column(b)) = (lhs.as_ref(), rhs.as_ref()) else {
        return None;
    };
    let left_slice = &combined_schema[..consumed_cols];
    if let (Some(l), Some(r)) = (
        Engine::composite_col_pos(left_slice, a),
        Engine::peer_col_pos(&peer.alias, &peer.cols, b),
    ) {
        return Some((l, r));
    }
    if let (Some(l), Some(r)) = (
        Engine::composite_col_pos(left_slice, b),
        Engine::peer_col_pos(&peer.alias, &peer.cols, a),
    ) {
        return Some((l, r));
    }
    None
}

/// v7.39 (round 588) — the WHERE conjuncts that could be equi-join keys.
///
/// `FROM a, b WHERE a.id = b.id` is the ANSI-89 spelling of
/// `FROM a JOIN b ON a.id = b.id` and means exactly the same join, but the
/// equality arrives in the WHERE clause: `analyze_join_pushdown` cannot place
/// it on either relation (its two qualifiers name two), and
/// `extract_join_keys` only ever read the ON clause. The peer was left with
/// no key at all and fell to the nested-loop stage, which crosses the ENTIRE
/// peer against every surviving left row.
///
/// The rewrite is only sound while every relation in the chain is
/// non-nullable — under an outer join a WHERE equality filters AFTER the
/// NULL-filling and is not the same thing as a join condition — so one outer
/// join anywhere gives up on the whole statement.
fn where_equi_candidates<'w>(from: &FromClause, where_: Option<&'w Expr>) -> Vec<&'w Expr> {
    let Some(w) = where_ else { return Vec::new() };
    if !from
        .joins
        .iter()
        .all(|j| matches!(j.kind, JoinKind::Inner | JoinKind::Cross))
    {
        return Vec::new();
    }
    reorder::split_and_conjunctions(w)
        .into_iter()
        .filter(|sub| {
            matches!(
                sub,
                Expr::Binary { lhs, op: spg_sql::ast::BinOp::Eq, rhs }
                    if matches!((lhs.as_ref(), rhs.as_ref()), (Expr::Column(_), Expr::Column(_)))
            )
        })
        .collect()
}

impl Engine {
    /// v7.17.0 Phase 3.P0-41 — build the per-peer descriptor for each
    /// join stage. A LATERAL peer can't be pre-materialised (its rows
    /// depend on outer columns), so it gets a sentinel carrying just
    /// the probed projection schema and the inner SELECT to re-run per
    /// outer row. A plain table with no pushed predicate is left
    /// deferred (the index-nested-loop path may avoid cloning it
    /// entirely). Everything else materialises eagerly to a
    /// (rows, schema) pair. `peer_preds[i]` are the WHERE conjuncts
    /// pushed onto peer `i` by `analyze_join_pushdown`.
    #[allow(clippy::type_complexity)]
    fn build_join_peers<'a>(
        &self,
        from: &'a FromClause,
        peer_preds: &[Vec<&Expr>],
        needed: Option<&alloc::collections::BTreeSet<(String, String)>>,
        budget: &mut ByteBudget,
    ) -> Result<Vec<JoinedPeer<'a>>, EngineError> {
        let mut joined: Vec<JoinedPeer<'a>> = Vec::new();
        for j in &from.joins {
            let a = j
                .table
                .alias
                .as_deref()
                .unwrap_or(j.table.name.as_str())
                .to_string();
            if let Some(inner_box) = &j.table.lateral_subquery {
                // v7.37 D.19 — a NON-correlated derived-table peer (a bare
                // `(VALUES …)`, which lowers to a UNION-ALL SELECT, or an
                // uncorrelated subquery) must be materialised ONCE as an eager
                // peer and cross-joined against every left row. Forcing it
                // through the per-left-row lateral path below dropped its
                // UNION-ALL rows, so `JOIN (VALUES ('1'),('2')) b ON true`
                // yielded only the first value per left row instead of the
                // full product. Only genuinely correlated laterals (which
                // reference an outer column) need per-left-row evaluation.
                // v7.39 (round 572) — …and that is what this now asks.
                //
                // The gate was `is_constant_values_derived`, which only
                // recognises a literal VALUES list, so an ordinary
                // uncorrelated derived table — `JOIN (SELECT … FROM t
                // WHERE …) b ON …`, as common a shape as SQL has — was
                // re-executed once per LEFT ROW. Measured on a 500k
                // table:
                //
                //     derived table alone                34.7 ms
                //     … as a join peer, 500 rows      8,552 ms
                //     … 2000 rows                    32,610 ms
                //     … 20000 rows                  >120,000 ms (cancelled)
                //     PG18, the 20000-row form           15.8 ms
                //
                // Linear in the LEFT side because the inner SELECT ran
                // again for each of its rows. `select_is_correlated` is
                // built to be wrong in the safe direction — its own
                // comment says a wrong "yes" costs only a
                // re-evaluation, while a wrong "no" is silently wrong —
                // so it is exactly the question to ask here.
                if is_constant_values_derived(inner_box)
                    || (derived_is_plain_table_select(inner_box, self.active_catalog())
                        && !crate::subquery::select_is_correlated(inner_box))
                {
                    let pidx = from
                        .joins
                        .iter()
                        .position(|jj| core::ptr::eq(jj, j))
                        .unwrap_or(0);
                    let (mut rows, mut cols) =
                        self.materialise_table_ref_filtered(&j.table, &peer_preds[pidx])?;
                    // `AS y(a, b)` renames positionally here exactly as
                    // it does on the per-left-row path below.
                    for (i, new_name) in j.table.unnest_column_aliases.iter().enumerate() {
                        if let Some(col) = cols.get_mut(i) {
                            col.name = new_name.clone();
                        }
                    }
                    if let Some(needed) = needed {
                        Self::null_out_unreferenced(&mut rows, &cols, &a, needed);
                    }
                    budget.charge(approx_rows_bytes(&rows))?;
                    joined.push(JoinedPeer {
                        eager_rows: Some(rows),
                        cols,
                        alias: a,
                        kind: j.kind,
                        on: j.on.as_ref(),
                        lateral: None,
                        join_table: None,
                        where_preds: Vec::new(),
                    });
                    continue;
                }
                // Probe schema by running the inner SELECT against a
                // NULL-padded outer context. The probe gives us the
                // projection's column shape; rows materialise per
                // left-row below.
                let mut schema = self.lateral_probe_schema(inner_box)?;
                // v7.37.16 — `AS y(a, b)` column-alias list renames the
                // derived table's columns positionally, exactly as the
                // FROM-primary derived-table path does (select.rs). The
                // probe returns the inner SELECT's own column names
                // (`column1` for a VALUES list, the inner projection
                // name for a subquery); without this rename a join
                // right-operand derived table left `y.a` unresolved
                // while `y.column1` / the inner name worked — a PG
                // divergence (PG applies the alias list identically in
                // FROM-primary and join-operand positions).
                for (i, new_name) in j.table.unnest_column_aliases.iter().enumerate() {
                    if let Some(col) = schema.get_mut(i) {
                        col.name = new_name.clone();
                    }
                }
                joined.push(JoinedPeer {
                    eager_rows: None,
                    cols: schema,
                    alias: a,
                    kind: j.kind,
                    on: j.on.as_ref(),
                    lateral: Some(inner_box.as_ref()),
                    join_table: None,
                    where_preds: Vec::new(),
                });
            } else {
                let pidx = from
                    .joins
                    .iter()
                    .position(|jj| core::ptr::eq(jj, j))
                    .unwrap_or(0);
                // v7.28 - defer materialisation for plain tables so the
                // index-nested-loop path can seek the driver and look up
                // only matched peer rows instead of cloning the whole
                // table. v7.33 — defer EVEN WITH a pushed WHERE predicate:
                // carry the predicate as `where_preds` for the stages to
                // apply as a residual on matched pairs (the eager path
                // here scanned + filtered the entire peer table, which on
                // mailrs's snippet subquery cost a full email_analysis scan
                // per seeked thread — 60× per IN-list group). Correctness
                // is backstopped by filter_join_survivors re-applying the
                // full WHERE to survivors.
                let plain = j.table.unnest_expr.is_none() && j.table.as_of_segment.is_none();
                if plain && let Some(t) = self.active_catalog().get(&j.table.name) {
                    // v7.34 (B5 ledger) — cost guard for 169ef66's INL
                    // pushdown: when the peer table is tiny AND a WHERE
                    // conjunct pushes onto it, the v7.28 eager path
                    // (scan + filter once, O(peer.rows + driver.rows))
                    // always beats INL (one peer-index seek + filter per
                    // driver row, O(driver.rows × log peer.rows + matched
                    // pair filter)). 169ef66 fixed mailrs's
                    // get_conversations IN(60) snippet subquery (peer
                    // 6k email_analysis, driver 25k messages — INL wins
                    // 13.7×), but regressed INBOX's outer mailboxes JOIN
                    // (peer = 30, driver = 25k — eager wins ~+4ms p50).
                    // SMALL_PEER_EAGER_ROWS at 256 keeps the IN(60) win
                    // (6k > 256 stays INL) while clawing back the
                    // small-peer case (30 ≤ 256 goes eager).
                    const SMALL_PEER_EAGER_ROWS: usize = 256;
                    let has_pushdown = !peer_preds[pidx].is_empty();
                    // v7.36 — drop the 7.35.1 force-eager-when-cold
                    // workaround. The downstream INL probe and hash
                    // build now thread cold-tier rows through
                    // `JoinSrc::Mixed` (PK-key map for INL;
                    // hash-iter Mixed.get for hash build). The
                    // nested-loop fallback's `lazy_rows` also
                    // appends cold rows. Small-peer + pushdown
                    // still takes the eager fast path.
                    let peer_total = t.rows().len();
                    if has_pushdown && peer_total <= SMALL_PEER_EAGER_ROWS {
                        let (mut rows, cols) =
                            self.materialise_table_ref_filtered(&j.table, &peer_preds[pidx])?;
                        if let Some(needed) = needed {
                            Self::null_out_unreferenced(&mut rows, &cols, &a, needed);
                        }
                        budget.charge(approx_rows_bytes(&rows))?;
                        joined.push(JoinedPeer {
                            eager_rows: Some(rows),
                            cols,
                            alias: a,
                            kind: j.kind,
                            on: j.on.as_ref(),
                            lateral: None,
                            join_table: Some(j.table.name.clone()),
                            where_preds: Vec::new(),
                        });
                        continue;
                    }
                    joined.push(JoinedPeer {
                        eager_rows: None,
                        cols: t.schema().columns.clone(),
                        alias: a,
                        kind: j.kind,
                        on: j.on.as_ref(),
                        lateral: None,
                        join_table: Some(j.table.name.clone()),
                        where_preds: peer_preds[pidx].iter().map(|e| (*e).clone()).collect(),
                    });
                    continue;
                }
                // Non-table peer (UNNEST / AS OF SEGMENT) — materialise
                // eagerly with its predicate filter applied up front.
                let (mut rows, cols) =
                    self.materialise_table_ref_filtered(&j.table, &peer_preds[pidx])?;
                if let Some(needed) = needed {
                    Self::null_out_unreferenced(&mut rows, &cols, &a, needed);
                }
                budget.charge(approx_rows_bytes(&rows))?;
                joined.push(JoinedPeer {
                    eager_rows: Some(rows),
                    cols,
                    alias: a,
                    kind: j.kind,
                    on: j.on.as_ref(),
                    lateral: None,
                    join_table: Some(j.table.name.clone()),
                    where_preds: Vec::new(),
                });
            }
        }
        Ok(joined)
    }

    pub(crate) fn build_joined_filtered_rows(
        &self,
        from: &FromClause,
        where_: Option<&Expr>,
        cancel: CancelToken<'_>,
        needed: Option<&alloc::collections::BTreeSet<(String, String)>>,
        budget: &mut ByteBudget,
    ) -> Result<DeferredJoin<'_>, EngineError> {
        let (swapped_from, primary_preds, peer_preds) = analyze_join_pushdown(from, where_);
        // v7.37.x (mailrs Track A perf — SPGE ≫ PG18) — pushed conjuncts
        // are enforced AT the primary `filter_table_indices` (or eager
        // peer `materialise_table_ref_filtered`) AND/OR as a join-stage
        // residual via `where_preds`. Re-applying them per joined tuple
        // inside `filter_join_survivors` is pure waste — 30 k tuples ×
        // compiled-WHERE eval cost ~1 ms on the mailrs minimal probe.
        // Build a residual WHERE = `where_ \ pushed_conjuncts` and pass
        // only that to the survivor filter. Identity is by `Expr` pointer
        // (analyze_join_pushdown gave us borrows into `where_`'s conjunct
        // set, so the pointers match exactly).
        // v7.39 (round 588) — the set grows during the peer loop below: a
        // WHERE equality promoted to a peer's join key is enforced BY the
        // join and must not be re-applied per survivor either.
        let mut pushed_set: alloc::collections::BTreeSet<usize> = primary_preds
            .iter()
            .chain(peer_preds.iter().flat_map(|v| v.iter()))
            .map(|e| core::ptr::from_ref::<Expr>(*e) as usize)
            .collect();
        let from = swapped_from.as_ref().unwrap_or(from);
        let primary_alias = from
            .primary
            .alias
            .as_deref()
            .unwrap_or(from.primary.name.as_str())
            .to_string();
        // v7.31 (perf campaign) — when the primary is a plain stored
        // table and there are joins to run, keep it in place: filter
        // to row indices (same index seek / linear filter) and let
        // the deferred-join pipeline clone only the surviving,
        // referenced columns once at output time. Joinless FROMs and
        // non-table refs take the materialising path.
        //
        // v7.30.3 byte-budget interplay: the index path materialises
        // nothing (row numbers are 8 B each), so the budget charges
        // land where the clones happen — the materialising fallback
        // here, eager peers below, and the output assembly.
        let primary_table: Option<&Table> = if !from.joins.is_empty()
            && from.primary.unnest_expr.is_none()
            && from.primary.lateral_subquery.is_none()
            && from.primary.as_of_segment.is_none()
        {
            self.active_catalog().get(&from.primary.name).filter(|t|
                // v7.36 (cold-tier coverage) — the deferred-index
                // primary path threads `Vec<usize>` row indices into
                // `JoinSrc::Stored(t.rows())` (hot-tier only), so a
                // primary with cold-tier rows silently dropped them
                // from the join. Force the materialising fallback
                // when ANY cold-tier row exists; the fallback rides
                // `materialise_table_ref_filtered` which already
                // covers both tiers (v7.35.1).
                !t.has_cold_rows_fast())
        } else {
            None
        };
        let (primary_rows, primary_cols, primary_indices) = match primary_table {
            Some(t) => {
                let idxs = self.filter_table_indices(t, &primary_alias, &primary_preds)?;
                // Phase C.3 step 2b — MVCC read gate on the deferred-index
                // primary seed. `idxs` are hot-tier physical indices into
                // `t.rows()` (this arm is reached only when the primary has
                // NO cold rows, see `has_cold_rows_fast()` filter above), so
                // dropping the invisible ones here keeps a dead/old version
                // out of the join without touching any cold-tier row. No-op
                // today: every hot header is frozen/committed-alive.
                let scan_snapshot = self.current_snapshot();
                let idxs: Vec<usize> = idxs
                    .into_iter()
                    .filter(|&i| t.is_row_visible(i, &scan_snapshot))
                    .collect();
                (Vec::new(), t.schema().columns.clone(), Some(idxs))
            }
            None => {
                let (mut rows, cols) =
                    self.materialise_table_ref_filtered(&from.primary, &primary_preds)?;
                if let Some(needed) = needed {
                    Self::null_out_unreferenced(&mut rows, &cols, &primary_alias, needed);
                }
                budget.charge(approx_rows_bytes(&rows))?;
                (rows, cols, None)
            }
        };
        let mut joined = self.build_join_peers(from, &peer_preds, needed, budget)?;
        let combined_schema = build_combined_schema(&primary_alias, &primary_cols, &joined);
        // v7.39 (read01 round 53) — the join's EvalContext must carry the
        // catalog. Without it a `::regclass` / enum / composite cast inside a
        // joined WHERE or ON falls back to plain text, so the canonical
        // `pg_class JOIN pg_index … WHERE indrelid = 't'::regclass` shape
        // errored on "comparison between BigInt and Text" — while the very
        // same predicate worked on a single-table SELECT (whose ctx does carry
        // the catalog). Same root as round 49's unnest(enum_range(…)).
        // v7.39 (round 525) — and the SESSION, for the same reason as the
        // catalog above: a join's WHERE is the same predicate a
        // single-table SELECT would carry, and `WHERE t =
        // current_setting('app.tenant')` failed on the joined shape while
        // working on the unjoined one.
        let join_sess = self.dml_session();
        let ctx = EvalContext::new(&combined_schema, None)
            .with_catalog(self.active_catalog())
            .with_session(&join_sess);
        if joined.is_empty() {
            // Joinless FROM: the primary rows ARE the combined rows —
            // filter and hand them back without any re-clone.
            let mut filtered: Vec<Row<'static>> = Vec::new();
            let mut memo = memoize::MemoizeCache::default();
            for row in primary_rows {
                if let Some(where_expr) = where_ {
                    let cond = self.eval_expr_with_correlated(
                        where_expr,
                        &row,
                        &ctx,
                        cancel,
                        Some(&mut memo),
                    )?;
                    if !crate::eval::predicate_is_true(&cond, "JOIN/ON", ctx.mysql_dialect)? {
                        continue;
                    }
                }
                filtered.push(row);
            }
            // v7.32 (P4 increment 2) — joinless: the survivors ARE the
            // primary rows; wrap them as one Owned source with identity
            // tuples so the deferred output type stays uniform.
            let width = combined_schema.len();
            let n = filtered.len();
            let offsets = alloc::vec![0, width];
            let pos_to_src = build_pos_to_src(&offsets);
            return Ok(DeferredJoin {
                sources: alloc::vec![JoinSrc::Owned(filtered)],
                offsets,
                pos_to_src,
                widths: alloc::vec![width],
                masks: alloc::vec![None],
                survivors: (0..n).collect(),
                stride: 1,
                combined_schema,
            });
        }
        // v7.31 (perf campaign) — deferred join materialisation: the
        // working set is a flat row-index tuple vec (stride = sources
        // joined so far, usize::MAX = a LEFT-join NULL slot), so a
        // combined Row materialises only where a residual-ON / lateral /
        // WHERE eval needs one and for the survivors handed back. Seed
        // the pipeline with the primary, then advance it one peer at a
        // time through the index-nested-loop, hash equi-join, or
        // nested-loop strategy.
        let primary_width = primary_cols.len();
        #[allow(clippy::type_complexity)]
        let (primary_source, primary_mask, working): (
            JoinSrc<'_>,
            Option<Vec<bool>>,
            Vec<usize>,
        ) = match primary_indices {
            Some(idxs) => {
                let t = primary_table.expect("stored primary");
                (
                    JoinSrc::Stored(t.rows()),
                    keep_mask(needed, &primary_cols, &primary_alias),
                    idxs,
                )
            }
            None => {
                let n = primary_rows.len();
                (JoinSrc::Owned(primary_rows), None, (0..n).collect())
            }
        };
        let where_equi = where_equi_candidates(from, where_);
        let mut pipe = JoinPipeline::new(primary_source, primary_mask, primary_width, working);
        for peer in &mut joined {
            if pipe.rows() > MAX_JOIN_INTERMEDIATE_ROWS {
                return Err(EngineError::Unsupported(alloc::format!(
                    "join intermediate result exceeds {MAX_JOIN_INTERMEDIATE_ROWS} rows ({} so far) - add join predicates",
                    pipe.rows()
                )));
            }
            let right_arity = peer.cols.len();
            let peer_mask = keep_mask(needed, &peer.cols, &peer.alias);
            let (mut eq_pairs, residual) =
                extract_join_keys(peer, &combined_schema, pipe.consumed_cols);
            // v7.39 (round 588) — an ANSI-89 join writes its condition in the
            // WHERE clause. Give the peer those keys too, so `FROM a, b WHERE
            // a.id = b.id` hashes exactly like `FROM a JOIN b ON a.id = b.id`
            // instead of crossing all of `b` against every row of `a`.
            if peer.lateral.is_none() && matches!(peer.kind, JoinKind::Inner | JoinKind::Cross) {
                for cand in &where_equi {
                    if let Some(pair) =
                        match_equi_pair(cand, peer, &combined_schema, pipe.consumed_cols)
                        && !eq_pairs.contains(&pair)
                    {
                        eq_pairs.push(pair);
                        pushed_set.insert(core::ptr::from_ref::<Expr>(*cand) as usize);
                    }
                }
            }
            // v7.33 — a deferred peer's pushed WHERE conjuncts ride as extra
            // residual so the INL / hash stages drop non-matching (left,
            // right) pairs in place (the eager path used to pre-filter the
            // whole peer). Taken out of `peer` so the &mut hash call below
            // doesn't alias the residual borrow.
            let extra_preds = core::mem::take(&mut peer.where_preds);
            let residual: Vec<&Expr> = residual.into_iter().chain(extra_preds.iter()).collect();
            if self.join_stage_inl(
                &mut pipe,
                peer,
                &eq_pairs,
                &residual,
                &peer_mask,
                right_arity,
                &ctx,
                cancel,
            )? {
                continue;
            }
            if !eq_pairs.is_empty() && peer.lateral.is_none() {
                self.join_stage_hash(
                    &mut pipe,
                    peer,
                    &eq_pairs,
                    &residual,
                    &peer_mask,
                    right_arity,
                    &combined_schema,
                    &ctx,
                    cancel,
                )?;
                continue;
            }
            self.join_stage_nested(
                &mut pipe,
                peer,
                right_arity,
                &combined_schema,
                &ctx,
                cancel,
                needed,
                budget,
            )?;
        }
        // v7.39 (round 588) — built here rather than before the loop because
        // `pushed_set` only learns about promoted equi-keys as each peer is
        // planned. A conjunct that failed to promote is still in the set's
        // complement and is still enforced, so a shape this does not
        // recognise keeps the old, correct behaviour.
        let residual_where_owned: Option<Expr> = where_.and_then(|w| {
            let kept: Vec<Expr> = reorder::split_and_conjunctions(w)
                .into_iter()
                .filter(|c| !pushed_set.contains(&(core::ptr::from_ref::<Expr>(c) as usize)))
                .cloned()
                .collect();
            kept.into_iter().reduce(|a, b| Expr::Binary {
                lhs: alloc::boxed::Box::new(a),
                op: spg_sql::ast::BinOp::And,
                rhs: alloc::boxed::Box::new(b),
            })
        });
        let survivors =
            self.filter_join_survivors(&pipe, residual_where_owned.as_ref(), &ctx, cancel, budget)?;
        Ok(DeferredJoin {
            sources: pipe.sources,
            offsets: pipe.offsets,
            pos_to_src: pipe.pos_to_src,
            widths: pipe.widths,
            masks: pipe.masks,
            survivors,
            stride: pipe.stride,
            combined_schema,
        })
    }

    /// v7.28 (round-22) — index-nested-loop join stage. When the working
    /// set is small and the peer's join column has a BTree, seek per left
    /// row instead of materialising the whole peer table (a correlated
    /// subquery body otherwise clones the full table once per outer
    /// group). Returns `Ok(false)` when the shape doesn't qualify, so the
    /// caller falls through to the hash / nested-loop strategy.
    #[allow(clippy::too_many_arguments)]
    fn join_stage_inl<'a, 'p>(
        &'a self,
        pipe: &mut JoinPipeline<'a>,
        peer: &JoinedPeer<'p>,
        eq_pairs: &[(usize, usize)],
        residual: &[&Expr],
        peer_mask: &Option<Vec<bool>>,
        right_arity: usize,
        ctx: &EvalContext,
        cancel: CancelToken<'_>,
    ) -> Result<bool, EngineError> {
        const INL_MAX_LEFT: usize = 1024;
        // v7.37.16 — RIGHT / FULL OUTER need to enumerate ALL peer rows
        // (to emit the unmatched ones with a NULL-filled left). The INL
        // probe only index-seeks the matched peer rows, so it cannot
        // produce the unmatched-right set. Bail to the hash stage, which
        // iterates every peer row and can track which matched.
        if matches!(peer.kind, JoinKind::Right | JoinKind::FullOuter) {
            return Ok(false);
        }
        let Some(tname) = &peer.join_table else {
            return Ok(false);
        };
        if !(peer.eager_rows.is_none() && !eq_pairs.is_empty() && pipe.rows() <= INL_MAX_LEFT) {
            return Ok(false);
        }
        let Some(table) = self.active_catalog().get(tname) else {
            return Ok(false);
        };
        let Some(idx) = peer
            .cols
            .iter()
            .position(|c| c.name == peer.cols[eq_pairs[0].1].name)
            .and_then(|pos| table.index_on(pos))
        else {
            return Ok(false);
        };
        // v7.36 — INL probe handles cold-tier locators only when the
        // peer's JOIN column is its single-column integer PRIMARY
        // KEY (the segment lookup is keyed by integer PK). For
        // non-PK JOINs on a cold-bearing peer, bail out so the
        // caller falls through to hash-join (which iterates the
        // peer via `Mixed` without needing locator-to-PK mapping).
        let has_cold = table.has_cold_rows_fast();
        let pk_col_pos = table
            .schema()
            .uniqueness_constraints
            .iter()
            .find(|u| u.is_primary_key && u.columns.len() == 1)
            .map(|u| u.columns[0]);
        let join_col_is_pk = pk_col_pos == Some(idx.column_position);
        if has_cold && !join_col_is_pk {
            return Ok(false);
        }
        let (cold_rows, cold_pk_map): (Vec<Row<'static>>, hashbrown::HashMap<i64, usize>) =
            if has_cold {
                crate::constraints::iter_cold_rows_with_locator_map(self.active_catalog(), table)
            } else {
                (Vec::new(), hashbrown::HashMap::new())
            };
        let stored = table.rows();
        let hot_len = stored.len();
        // Phase C.3 step 2b — MVCC read gate for the INL probe. Snapshot
        // computed once; a hot peer row (`ri < hot_len`) this snapshot
        // cannot see is skipped so it never matches. Cold rows
        // (`ri >= hot_len`) are frozen segment rows = always visible.
        // No-op today: every hot header is frozen/committed-alive.
        let scan_snapshot = self.current_snapshot();
        let (lpos0, _) = eq_pairs[0];
        let mut next: Vec<usize> = Vec::new();
        for tuple in pipe.working.chunks(pipe.stride) {
            cancel.check()?;
            let mut left_matched = false;
            if let Some(kv) = tuple_value(&pipe.sources, &pipe.offsets, tuple, lpos0)
                && !matches!(kv, Value::Null)
                && let Some(key) = spg_storage::IndexKey::from_value(kv)
            {
                for loc in idx.lookup_eq(&key) {
                    let ri = match *loc {
                        spg_storage::RowLocator::Hot(i) => i,
                        spg_storage::RowLocator::Cold { .. } => {
                            // Mixed-eligible branch (PK BTree
                            // lookup). The locator's key equals the
                            // PK key here; use it to find the row in
                            // `cold_rows` via `cold_pk_map`.
                            let spg_storage::IndexKey::Int(pk) = &key else {
                                continue;
                            };
                            match cold_pk_map.get(pk) {
                                Some(&off) => hot_len + off,
                                None => continue,
                            }
                        }
                    };
                    let right_opt: Option<&Row<'static>> = if ri < hot_len {
                        if !table.is_row_visible(ri, &scan_snapshot) {
                            continue;
                        }
                        stored.get(ri)
                    } else {
                        cold_rows.get(ri - hot_len)
                    };
                    let right = match right_opt {
                        Some(r) => r,
                        None => continue,
                    };
                    // Remaining eq pairs + residual ON check on the
                    // candidate only.
                    let mut ok = true;
                    for (lp, rp) in eq_pairs.iter().skip(1) {
                        let lv = tuple_value(&pipe.sources, &pipe.offsets, tuple, *lp);
                        let rv = right.values.get(*rp);
                        let eq = match (lv, rv) {
                            (Some(a), Some(b)) => {
                                !matches!(a, Value::Null)
                                    && !matches!(b, Value::Null)
                                    && value_cmp(a, b) == core::cmp::Ordering::Equal
                            }
                            _ => false,
                        };
                        if !eq {
                            ok = false;
                            break;
                        }
                    }
                    if !ok {
                        continue;
                    }
                    let keep = if residual.is_empty() {
                        true
                    } else {
                        let mut combined_vals = materialise_tuple_vals(
                            &pipe.sources,
                            &pipe.widths,
                            &pipe.masks,
                            tuple,
                            pipe.consumed_cols + right_arity,
                        );
                        extend_masked(&mut combined_vals, right, peer_mask.as_deref());
                        let combined = Row::new(combined_vals);
                        let mut k = true;
                        for r in residual {
                            let cond =
                                self.eval_expr_with_correlated(r, &combined, ctx, cancel, None)?;
                            if !crate::eval::predicate_is_true(&cond, "JOIN/ON", ctx.mysql_dialect)? {
                                k = false;
                                break;
                            }
                        }
                        k
                    };
                    if keep {
                        next.extend_from_slice(tuple);
                        next.push(ri);
                        left_matched = true;
                    }
                }
            }
            if !left_matched && matches!(peer.kind, JoinKind::Left) {
                next.extend_from_slice(tuple);
                next.push(usize::MAX);
            }
        }
        let src = if cold_rows.is_empty() {
            JoinSrc::Stored(stored)
        } else {
            JoinSrc::Mixed {
                hot: stored,
                cold: cold_rows,
                cold_locator_map: cold_pk_map,
            }
        };
        pipe.advance(next, src, peer_mask.clone(), right_arity);
        Ok(true)
    }

    /// v7.28 (round-22) — hash equi-join stage. The naive path cloned the
    /// full combined row for EVERY (left, right) pair before evaluating
    /// ON — O(L×R) materialisations (a 24k × 6k LEFT JOIN never returned).
    /// Build a hash on the (smaller) right side over the `eq_pairs` keys,
    /// probe per left tuple, and materialise only matching pairs for the
    /// `residual` ON conjuncts. NULL keys never match (SQL equality).
    #[allow(clippy::too_many_arguments)]
    fn join_stage_hash<'a, 'p>(
        &'a self,
        pipe: &mut JoinPipeline<'a>,
        peer: &mut JoinedPeer<'p>,
        eq_pairs: &[(usize, usize)],
        residual: &[&Expr],
        peer_mask: &Option<Vec<bool>>,
        right_arity: usize,
        combined_schema: &[ColumnSchema],
        ctx: &EvalContext,
        cancel: CancelToken<'_>,
    ) -> Result<(), EngineError> {
        // Build side: eager rows if the peer was materialised (pushed
        // predicate / non-table ref), otherwise the stored table read in
        // place (v7.31 — no full-table clone + null-out just to hash it).
        // v7.32 (P4 increment 2) — move the eager build side into an
        // Owned source instead of borrowing `peer`, so the deferred
        // output can outlive this stage. Probe and hash-build read the
        // local `rights_src`.
        // Phase C.3 step 2b — MVCC read gate for the hash build side.
        // `build_gate` carries the peer `Table` + its hot-row count when
        // the build source is backed by the hot tier (`Stored`, or the
        // hot prefix of `Mixed`); a build row `ri < hot_len` this
        // snapshot cannot see is skipped so it never enters a bucket.
        // `Owned` build rows were already materialised (and filtered)
        // upstream, so they carry no physical hot index and stay ungated;
        // cold rows (`ri >= hot_len` in `Mixed`) are frozen = visible.
        // No-op today: every hot header is frozen/committed-alive.
        let (rights_src, build_gate): (JoinSrc<'a>, Option<(&'a Table, usize)>) =
            match peer.eager_rows.take() {
                Some(rows) => (JoinSrc::Owned(rows), None),
                None => match peer
                    .join_table
                    .as_deref()
                    .and_then(|n| self.active_catalog().get(n))
                {
                    // v7.36 — cold-bearing peer hashes through `Mixed`.
                    // Unlike INL, hash build doesn't consume the
                    // locator's key; it iterates the source via
                    // `len()/get()` and indexes each row by its
                    // eq_pairs values — works correctly for ANY join
                    // column (PK or secondary). No PK constraint.
                    Some(t) if t.has_cold_rows_fast() => {
                        let (cold, map) = crate::constraints::iter_cold_rows_with_locator_map(
                            self.active_catalog(),
                            t,
                        );
                        let hot = t.rows();
                        let hot_len = hot.len();
                        (
                            JoinSrc::Mixed {
                                hot,
                                cold,
                                cold_locator_map: map,
                            },
                            Some((t, hot_len)),
                        )
                    }
                    Some(t) => (JoinSrc::Stored(t.rows()), Some((t, t.rows().len()))),
                    None => (JoinSrc::Owned(Vec::new()), None),
                },
            };
        let scan_snapshot = self.current_snapshot();
        let n_rights = rights_src.len();
        // v7.29 - hashbrown over BTreeMap: the ordered map paid
        // O(log n) string comparisons per insert/probe (24k-row build
        // sides spent ~100 ms in it).
        // v7.36 (perf — mailrs Phase 1) — type-specialised i64 hash
        // table when the join keys are a single integer column on
        // both sides (the overwhelming case: FK to PK joins, ID
        // lookups). Skips the `encode_one` → String round-trip
        // entirely; the hash key is the i64 itself. For count_messages
        // / inbox / contacts / list_categories the eq_pair is
        // `(messages.mailbox_id, mailboxes.id)` both BigInt.
        let int_keyed = eq_pairs.len() == 1
            && matches!(
                combined_schema[eq_pairs[0].0].ty,
                spg_storage::DataType::BigInt
                    | spg_storage::DataType::Int
                    | spg_storage::DataType::SmallInt
            )
            && {
                let peer_col_ty = peer.cols.get(eq_pairs[0].1).map(|c| c.ty);
                matches!(
                    peer_col_ty,
                    Some(
                        spg_storage::DataType::BigInt
                            | spg_storage::DataType::Int
                            | spg_storage::DataType::SmallInt
                    )
                )
            };
        let mut table: hashbrown::HashMap<String, Bucket> =
            hashbrown::HashMap::with_capacity(if int_keyed { 0 } else { n_rights });
        let mut int_table: hashbrown::HashMap<i64, Bucket> =
            hashbrown::HashMap::with_capacity(if int_keyed { n_rights } else { 0 });
        let mut keybuf: Vec<&Value> = Vec::with_capacity(eq_pairs.len());
        // v7.31 (perf 3e) — scratch key buffer: build inserts allocate
        // only on vacant, probes never allocate.
        let mut keystr = String::new();
        'build: for ri in 0..n_rights {
            if let Some((gt, hot_len)) = build_gate
                && ri < hot_len
                && !gt.is_row_visible(ri, &scan_snapshot)
            {
                continue;
            }
            let Some(right) = rights_src.get(ri) else {
                continue;
            };
            if int_keyed {
                let rpos = eq_pairs[0].1;
                let key = match right.values.get(rpos) {
                    Some(Value::BigInt(n)) => *n,
                    Some(Value::Int(n)) => i64::from(*n),
                    Some(Value::SmallInt(n)) => i64::from(*n),
                    _ => continue 'build,
                };
                // v7.37.x (docker-fair NOTEX hash-build attack) — most
                // FK-to-PK joins are unique on the build side, so the
                // bucket is a one-element Vec. `or_default()` lands as
                // a 0-cap Vec then the push grows it through 1 → 4
                // (two allocs); pre-sizing to 1 cuts those to one.
                // For the NOTEX 12.5 k-row build side this saves
                // ~12.5 k × ~100 ns ≈ 1.25 ms per query.
                match int_table.entry(key) {
                    hashbrown::hash_map::Entry::Occupied(mut o) => o.get_mut().push(ri),
                    hashbrown::hash_map::Entry::Vacant(v) => {
                        v.insert(Bucket::One(ri));
                    }
                }
                continue;
            }
            keybuf.clear();
            for (_, rpos) in eq_pairs {
                match right.values.get(*rpos) {
                    Some(v) if !matches!(v, Value::Null) => keybuf.push(v),
                    _ => continue 'build,
                }
            }
            aggregate::encode_key_refs_into(&keybuf, &mut keystr);
            match table.get_mut(keystr.as_str()) {
                Some(b) => b.push(ri),
                None => {
                    table.insert(keystr.clone(), Bucket::One(ri));
                }
            }
        }
        let mut next: Vec<usize> = Vec::new();
        // v7.37.16 — RIGHT / FULL OUTER: track which peer (build-side)
        // rows joined with at least one drive tuple so the unmatched
        // ones can be emitted (NULL-filled left) after the probe loop.
        // Empty (unallocated) for INNER / LEFT — no per-row cost there.
        let track_right = matches!(peer.kind, JoinKind::Right | JoinKind::FullOuter);
        let mut peer_matched: Vec<bool> = if track_right {
            alloc::vec![false; n_rights]
        } else {
            Vec::new()
        };
        let mut probebuf: Vec<&Value> = Vec::with_capacity(eq_pairs.len());
        for tuple in pipe.working.chunks(pipe.stride) {
            cancel.check()?;
            let mut left_matched = false;
            let mut left_has_null = false;
            let int_probe_key: Option<i64> = if int_keyed {
                let lpos = eq_pairs[0].0;
                match tuple_value(&pipe.sources, &pipe.offsets, tuple, lpos) {
                    Some(Value::BigInt(n)) => Some(*n),
                    Some(Value::Int(n)) => Some(i64::from(*n)),
                    Some(Value::SmallInt(n)) => Some(i64::from(*n)),
                    _ => {
                        left_has_null = true;
                        None
                    }
                }
            } else {
                probebuf.clear();
                for (lpos, _) in eq_pairs {
                    match tuple_value(&pipe.sources, &pipe.offsets, tuple, *lpos) {
                        Some(v) if !matches!(v, Value::Null) => probebuf.push(v),
                        _ => {
                            left_has_null = true;
                            break;
                        }
                    }
                }
                if !left_has_null {
                    aggregate::encode_key_refs_into(&probebuf, &mut keystr);
                }
                None
            };
            let cands_opt: Option<&Bucket> = if left_has_null {
                None
            } else if int_keyed {
                int_table.get(&int_probe_key.unwrap())
            } else {
                table.get(keystr.as_str())
            };
            if let Some(cands) = cands_opt {
                for &ri in cands.as_slice() {
                    let keep = if residual.is_empty() {
                        true
                    } else {
                        let right = rights_src.get(ri).expect("hash candidate row");
                        let mut combined_vals = materialise_tuple_vals(
                            &pipe.sources,
                            &pipe.widths,
                            &pipe.masks,
                            tuple,
                            pipe.consumed_cols + right_arity,
                        );
                        extend_masked(&mut combined_vals, right, peer_mask.as_deref());
                        let combined = Row::new(combined_vals);
                        let mut ok = true;
                        for r in residual {
                            let cond =
                                self.eval_expr_with_correlated(r, &combined, ctx, cancel, None)?;
                            if !crate::eval::predicate_is_true(&cond, "JOIN/ON", ctx.mysql_dialect)? {
                                ok = false;
                                break;
                            }
                        }
                        ok
                    };
                    if keep {
                        next.extend_from_slice(tuple);
                        next.push(ri);
                        left_matched = true;
                        if track_right {
                            peer_matched[ri] = true;
                        }
                    }
                }
            }
            // LEFT (and FULL OUTER) keep unmatched drive rows with a
            // NULL-filled peer (`usize::MAX` sentinel → NULL columns).
            if !left_matched && matches!(peer.kind, JoinKind::Left | JoinKind::FullOuter) {
                next.extend_from_slice(tuple);
                next.push(usize::MAX);
            }
        }
        // v7.37.16 — RIGHT / FULL OUTER: append the peer rows that no
        // drive tuple matched, NULL-filling every prior source column
        // (a `usize::MAX` sentinel for each of the `stride` drive slots)
        // and carrying the real peer row index. Same visibility gate as
        // the build loop so an invisible / NULL-key peer row is emitted
        // once and only when it truly exists.
        if track_right {
            for ri in 0..n_rights {
                if peer_matched[ri] {
                    continue;
                }
                if let Some((gt, hot_len)) = build_gate
                    && ri < hot_len
                    && !gt.is_row_visible(ri, &scan_snapshot)
                {
                    continue;
                }
                if rights_src.get(ri).is_none() {
                    continue;
                }
                for _ in 0..pipe.stride {
                    next.push(usize::MAX);
                }
                next.push(ri);
            }
        }
        pipe.advance(next, rights_src, peer_mask.clone(), right_arity);
        debug_assert!(pipe.consumed_cols <= combined_schema.len());
        Ok(())
    }

    /// Nested-loop join stage — the fallback for LATERAL peers and
    /// non-equi ON. A deferred plain-table peer materialises here
    /// (pruned), since every (left, right) pair gets evaluated anyway.
    #[allow(clippy::too_many_arguments)]
    fn join_stage_nested<'a, 'p>(
        &'a self,
        pipe: &mut JoinPipeline<'a>,
        peer: &mut JoinedPeer<'p>,
        right_arity: usize,
        combined_schema: &[ColumnSchema],
        ctx: &EvalContext,
        cancel: CancelToken<'_>,
        needed: Option<&alloc::collections::BTreeSet<(String, String)>>,
        budget: &mut ByteBudget,
    ) -> Result<(), EngineError> {
        let lazy_rows: Option<Vec<Row<'static>>> =
            if peer.eager_rows.is_none() && peer.lateral.is_none() {
                let tname = peer.join_table.as_deref().unwrap_or("");
                // v7.37.15 Phase B — visibility-gated nested-loop
                // fallback peer scan.
                let snap = self.current_snapshot();
                let mut rows: Vec<Row<'static>> = self
                    .active_catalog()
                    .get(tname)
                    .map(|t| t.scan_visible(&snap).map(|(_, r)| r.clone()).collect())
                    .unwrap_or_default();
                // v7.36 — nested-loop fallback materialises the peer
                // into `lazy_rows`. Append cold-tier rows so the fall-
                // back stays correct after the force-eager-when-cold
                // guard was lifted in `build_join_peers`.
                if let Some(t) = self.active_catalog().get(tname)
                    && t.has_cold_rows_fast()
                {
                    rows.extend(crate::constraints::iter_cold_rows_of_parent(
                        self.active_catalog(),
                        t,
                    ));
                }
                if let Some(needed) = needed {
                    Self::null_out_unreferenced(&mut rows, &peer.cols, &peer.alias, needed);
                }
                budget.charge(approx_rows_bytes(&rows))?;
                Some(rows)
            } else {
                None
            };
        // Lateral results are per-outer-row, so matched right rows persist
        // in a stage arena the tuples can index.
        let mut arena: Vec<Row<'static>> = Vec::new();
        let rights_eager: Option<&[Row<'static>]> =
            peer.eager_rows.as_deref().or(lazy_rows.as_deref());
        let mut next: Vec<usize> = Vec::new();
        let right_or_full = matches!(peer.kind, JoinKind::Right | JoinKind::FullOuter);
        // v7.37.16 — RIGHT / FULL OUTER over a *derived-table* right
        // operand (VALUES / non-correlated subquery). `build_join_peers`
        // routes every derived table through the "lateral" branch even
        // when it is not correlated; materialise it ONCE here into a
        // fixed row set so the unmatched-peer rows can be enumerated. A
        // truly correlated peer would give per-left-row-varying rows, but
        // PG rejects RIGHT/FULL LATERAL, so a single NULL-outer
        // materialisation is the correct fixed set. Also handles the
        // empty-drive case (no left tuples → every peer row unmatched).
        let lateral_fixed: Option<Vec<Row<'static>>> =
            if right_or_full && let Some(inner) = peer.lateral {
                // Materialise the derived table directly (no outer-column
                // substitution): a RIGHT/FULL peer must be non-correlated
                // (PG rejects RIGHT/FULL LATERAL), so its rows are the same
                // for every drive row and independent of the outer context.
                // Use the union-aware entry: a multi-row `VALUES (…),(…)` is
                // stored as a head SELECT + `stmt.unions` tails, so the bare
                // (non-union) executor would return only the first row.
                match self.exec_select_cancel(inner, cancel)? {
                    QueryResult::Rows { rows, .. } => Some(rows),
                    _ => {
                        return Err(EngineError::Unsupported(
                            "derived-table join operand must be a SELECT".into(),
                        ));
                    }
                }
            } else {
                None
            };
        // A fixed peer-row index space exists for the non-lateral eager
        // path OR the just-materialised `lateral_fixed` set. Both let
        // RIGHT / FULL OUTER track which peer rows matched and emit the
        // unmatched ones (NULL-filled left) after the loop.
        let track_right = right_or_full && (peer.lateral.is_none() || lateral_fixed.is_some());
        let fixed_peer_len = match &lateral_fixed {
            Some(f) => f.len(),
            None => rights_eager.map(<[_]>::len).unwrap_or(0),
        };
        let mut peer_matched: Vec<bool> = if track_right {
            alloc::vec![false; fixed_peer_len]
        } else {
            Vec::new()
        };
        for tuple in pipe.working.chunks(pipe.stride) {
            cancel.check()?;
            let mut left_matched = false;
            let left_vals = materialise_tuple_vals(
                &pipe.sources,
                &pipe.widths,
                &pipe.masks,
                tuple,
                pipe.consumed_cols,
            );
            let per_left_rrows: Cow<'_, [Row]> = match (&lateral_fixed, peer.lateral) {
                // RIGHT/FULL derived-table peer — the single fixed set.
                (Some(fixed), _) => Cow::Borrowed(fixed.as_slice()),
                (None, Some(inner)) => {
                    // Substitute outer columns and run the inner SELECT
                    // against the current left row's slice of the
                    // combined schema.
                    let outer_schema = &combined_schema[..pipe.consumed_cols];
                    let left_row = Row::new(left_vals.clone());
                    let rows =
                        self.materialise_lateral_for_outer(inner, outer_schema, &left_row)?;
                    Cow::Owned(rows)
                }
                (None, None) => Cow::Borrowed(rights_eager.expect("non-lateral peer eager")),
            };
            for (ri, right) in per_left_rrows.as_ref().iter().enumerate() {
                let mut combined_vals = left_vals.clone();
                combined_vals.extend(right.values.iter().cloned());
                let combined = Row::new(combined_vals);
                let keep = if let Some(on_expr) = peer.on {
                    // v7.24.1 — correlated-aware (subqueries in ON
                    // referencing earlier join columns).
                    let cond =
                        self.eval_expr_with_correlated(on_expr, &combined, ctx, cancel, None)?;
                    crate::eval::predicate_is_true(&cond, "JOIN/ON", ctx.mysql_dialect)?
                } else {
                    true
                };
                if keep {
                    next.extend_from_slice(tuple);
                    if peer.lateral.is_some() && lateral_fixed.is_none() {
                        // Correlated / INNER-LEFT lateral: per-outer-row
                        // arena (rows vary per left tuple).
                        let mut cv = combined.values;
                        let rv = cv.split_off(left_vals.len());
                        arena.push(Row::new(rv));
                        next.push(arena.len() - 1);
                    } else {
                        // Fixed peer index space (non-lateral eager, or
                        // the RIGHT/FULL `lateral_fixed` set).
                        next.push(ri);
                        if track_right {
                            peer_matched[ri] = true;
                        }
                    }
                    left_matched = true;
                }
            }
            if !left_matched && matches!(peer.kind, JoinKind::Left | JoinKind::FullOuter) {
                next.extend_from_slice(tuple);
                next.push(usize::MAX);
            }
        }
        // v7.37.16 — RIGHT / FULL OUTER: append unmatched peer rows with
        // a NULL-filled left (`usize::MAX` for each drive slot).
        if track_right {
            for (ri, matched) in peer_matched.iter().enumerate() {
                if *matched {
                    continue;
                }
                for _ in 0..pipe.stride {
                    next.push(usize::MAX);
                }
                next.push(ri);
            }
        }
        if next.len() / (pipe.stride + 1) > MAX_JOIN_INTERMEDIATE_ROWS {
            return Err(EngineError::Unsupported(alloc::format!(
                "join intermediate result exceeds {MAX_JOIN_INTERMEDIATE_ROWS} rows ({} so far) - add join predicates",
                next.len() / (pipe.stride + 1)
            )));
        }
        let source = if let Some(fixed) = lateral_fixed {
            // RIGHT/FULL derived-table peer — the fixed set is the source.
            JoinSrc::Owned(fixed)
        } else if peer.lateral.is_some() {
            JoinSrc::Owned(arena)
        } else if let Some(lz) = lazy_rows {
            JoinSrc::Owned(lz)
        } else {
            // v7.32 (P4 increment 2) — move (not borrow) the eager peer
            // rows; `rights_eager` has finished its nested-loop borrow.
            JoinSrc::Owned(peer.eager_rows.take().expect("non-lateral peer eager"))
        };
        // Fallback sources are pre-pruned (eager / lazy null-out) or
        // lateral projections; nothing left for a mask to drop.
        pipe.advance(next, source, None, right_arity);
        debug_assert!(pipe.consumed_cols <= combined_schema.len());
        Ok(())
    }

    /// v7.24 (round-16 B) — final WHERE filter over the joined working
    /// set. The compiled path reads cells by reference through
    /// `RowRef::Tuple` (`eval_compiled_ref`) WITHOUT materialising a
    /// combined Row; only a correlated WHERE (subqueries) materialises,
    /// once, per surviving probe, through the memoized correlated-aware
    /// evaluator. Survivors are returned as their row-index tuples — the
    /// aggregate path borrows them, projection / window callers
    /// `materialise()`.
    fn filter_join_survivors(
        &self,
        pipe: &JoinPipeline<'_>,
        where_: Option<&Expr>,
        ctx: &EvalContext,
        cancel: CancelToken<'_>,
        budget: &mut ByteBudget,
    ) -> Result<Vec<usize>, EngineError> {
        // v7.37.x (mailrs Track A perf — paired with v7.37.15
        // pushdown-strip) — when every conjunct was pushed onto its
        // source (eager peer filter / primary index seek / join-stage
        // residual), `residual_where` is None and every joined tuple
        // is already a survivor. Skip the per-tuple eval-or-true loop:
        // budget-charge a single rectangular approximation of the
        // whole working set, then bulk-copy the tuple indices via
        // `to_vec()`. On the mailrs minimal 100k shape this turns a
        // 100 k-iter per-tuple loop into a single allocation +
        // memcpy.
        if where_.is_none() {
            // Approximate total bytes by per-row cost × row count
            // (mirrors what the per-tuple charge would sum). Empty
            // working set short-circuits to a no-op.
            let n_rows = if pipe.stride == 0 {
                0
            } else {
                pipe.working.len() / pipe.stride
            };
            if n_rows > 0 {
                let sample_tuple = &pipe.working[..pipe.stride];
                let per_tuple =
                    approx_tuple_bytes(&pipe.sources, &pipe.offsets, &pipe.masks, sample_tuple);
                budget.charge(per_tuple.saturating_mul(n_rows))?;
            }
            cancel.check()?;
            return Ok(pipe.working.clone());
        }
        let mut memo = memoize::MemoizeCache::default();
        let compiled_where: Option<eval::CompiledExpr> = where_
            .filter(|w| eval::fully_compilable(w))
            .map(|w| eval::compile_expr(w, ctx));
        let mut survivors: Vec<usize> = Vec::new();
        for tuple in pipe.working.chunks(pipe.stride) {
            let rr = RowRef::Tuple {
                sources: &pipe.sources,
                offsets: &pipe.offsets,
                pos_to_src: &pipe.pos_to_src,
                tuple,
            };
            // v7.37.9 T3 S2 — declare eval_stack inside the per-tuple
            // loop so its `'val` lifetime binds to `rr`'s local scope.
            // The Vec allocation per tuple is amortised by the row's
            // existing work; alternative (outer-scope Vec) hits the
            // lifetime-contamination wall under `'row: 'val`.
            let mut eval_stack: Vec<Value<'_>> = Vec::new();
            let pass = if let Some(cw) = &compiled_where {
                matches!(
                    eval::eval_compiled_ref(cw, &rr, ctx, &mut eval_stack)
                        .map_err(EngineError::Eval)?,
                    Value::Bool(true)
                )
            } else if let Some(where_expr) = where_ {
                let row = rr.as_row();
                matches!(
                    self.eval_expr_with_correlated(where_expr, &row, ctx, cancel, Some(&mut memo))?,
                    Value::Bool(true)
                )
            } else {
                true
            };
            if !pass {
                continue;
            }
            // v7.30.3 byte budget — survivors hold 8 B row numbers, but
            // the live data they reference is what the meter must track;
            // `approx_tuple_bytes` sums it by reference (no clone),
            // mirroring the bytes the old materialised path charged.
            budget.charge(approx_tuple_bytes(
                &pipe.sources,
                &pipe.offsets,
                &pipe.masks,
                tuple,
            ))?;
            survivors.extend_from_slice(tuple);
        }
        Ok(survivors)
    }

    /// v7.17.0 Phase 3.P0-41 — probe a LATERAL subquery's projection
    /// schema by running it once with a NULL-padded outer context.
    /// The probe never materialises real outer rows; it just executes
    /// the inner SELECT with `outer_alias.col` references substituted
    /// to NULL so the projection's type inference is exercised.
    fn lateral_probe_schema(
        &self,
        inner: &SelectStatement,
    ) -> Result<Vec<ColumnSchema>, EngineError> {
        // Substitute every qualified column reference whose qualifier
        // does NOT match an in-subquery FROM alias with NULL. The
        // safest probe is to walk the inner SELECT and replace any
        // `<qual>.<col>` whose qual isn't bound inside the subquery
        // with a Null literal. For the v7.17 probe we just run the
        // unmodified subquery and surface the columns; if it fails
        // (e.g. references an outer column the probe can't resolve),
        // we synthesise a best-effort schema from the SELECT items
        // by inferring a single Text-typed column per projection.
        match self.execute_readonly_select_for_lateral_probe(inner) {
            Ok(QueryResult::Rows { columns, .. }) => Ok(columns),
            // Best-effort fallback: each SELECT item becomes a TEXT
            // column. Real schemas only differ when the inner SELECT
            // references outer columns at projection-time; those
            // queries surface via the substitution path during
            // per-row execution and still return the right values.
            _ => {
                // `SELECT * FROM <srf>(… outer.col …)` — the wrapped
                // correlated-SRF shape. The probe can't evaluate the
                // outer reference, but the SRF ref itself dictates
                // the schema: column-alias list first, then the
                // executor's natural defaults (alias / fn name), plus
                // the WITH ORDINALITY counter.
                if let [SelectItem::Wildcard] = inner.items.as_slice()
                    && let Some(from) = &inner.from
                    && from.joins.is_empty()
                    && (from.primary.unnest_expr.is_some()
                        || from.primary.generate_series_args.is_some())
                {
                    let t = &from.primary;
                    let elem_dtype = if t.generate_series_args.is_some() {
                        DataType::BigInt
                    } else {
                        DataType::Text
                    };
                    let first = t
                        .unnest_column_aliases
                        .first()
                        .cloned()
                        .or_else(|| t.alias.clone())
                        .unwrap_or_else(|| t.name.clone());
                    let mut out = alloc::vec![ColumnSchema::new(first, elem_dtype, true)];
                    if t.with_ordinality {
                        let ord = t
                            .unnest_column_aliases
                            .get(1)
                            .cloned()
                            .unwrap_or_else(|| "ordinality".to_string());
                        out.push(ColumnSchema::new(ord, DataType::BigInt, false));
                    }
                    return Ok(out);
                }
                // v7.39 (read01 round 69) — `SELECT * FROM <user fn>(…)`, which is
                // what a correlated `LATERAL f(t.c)` wraps into. A wildcard has no
                // name to give, so without this the column came back as `col0` and
                // the alias (`AS d`) resolved to nothing. Take the shape the
                // function DECLARES: `RETURNS TABLE(id int, v text)` names its
                // columns, and a `SETOF <scalar>` is one column named after the
                // call's alias.
                // v7.39 (round 205, JSON_TABLE) — a wrapped correlated
                // JSON_TABLE (`SELECT * FROM JSON_TABLE(t.col, …)`):
                // its column shape is STATIC (COLUMNS list), so infer
                // it directly without evaluating the doc (which still
                // references the outer column at schema time).
                if let Some(from) = &inner.from
                    && from.joins.is_empty()
                    && matches!(inner.items.as_slice(), [SelectItem::Wildcard])
                    && let Some(jt) = from.primary.json_table.as_deref()
                {
                    return Ok(crate::select::json_table_schema_pub(&jt.columns));
                }
                if let Some(from) = &inner.from
                    && from.joins.is_empty()
                    && matches!(inner.items.as_slice(), [SelectItem::Wildcard])
                    && let Some((fn_name, _)) = from.primary.table_fn_call.as_deref()
                {
                    let cat = self.active_catalog();
                    let overloads = cat.functions_named(fn_name);
                    if let Some(def) = overloads.first() {
                        let declared = def.returns.trim();
                        let upper = declared.to_ascii_uppercase();
                        if let Some(rest) = upper.strip_prefix("TABLE(") {
                            let _ = rest;
                            let raw = &declared["TABLE(".len()..declared.len() - 1];
                            let cols: Vec<ColumnSchema> = raw
                                .split(',')
                                .map(|decl| {
                                    let cname = decl.split_whitespace().next().unwrap_or("col");
                                    ColumnSchema::new(cname.to_string(), DataType::Text, true)
                                })
                                .collect();
                            return Ok(cols);
                        }
                        let cname = from
                            .primary
                            .alias
                            .clone()
                            .unwrap_or_else(|| fn_name.clone());
                        return Ok(alloc::vec![ColumnSchema::new(cname, DataType::Text, true)]);
                    }
                }
                let mut out: Vec<ColumnSchema> = Vec::new();
                for (i, item) in inner.items.iter().enumerate() {
                    let name = match item {
                        SelectItem::Expr { alias: Some(a), .. } => a.clone(),
                        SelectItem::Expr { expr, .. } => synth_lateral_col_name(expr, i),
                        SelectItem::Wildcard | SelectItem::QualifiedWildcard(_) => {
                            alloc::format!("col{i}")
                        }
                    };
                    out.push(ColumnSchema::new(name, DataType::Text, true));
                }
                Ok(out)
            }
        }
    }

    /// v7.17.0 Phase 3.P0-41 — try the inner LATERAL subquery against
    /// the engine in read-only mode for schema-probe purposes. Failure
    /// is expected when the subquery references an outer column the
    /// probe can't resolve; the caller falls back to a best-effort
    /// schema based on the SELECT items.
    fn execute_readonly_select_for_lateral_probe(
        &self,
        inner: &SelectStatement,
    ) -> Result<QueryResult, EngineError> {
        self.exec_bare_select_cancel(inner, CancelToken::none())
    }

    /// v7.17.0 Phase 3.P0-41 — materialise a LATERAL subquery's rows
    /// for one outer-row context. Walks the inner SELECT, replaces
    /// every `<outer_alias>.<col>` reference whose alias appears in
    /// the outer schema with the literal value from the outer row,
    /// then runs the rewritten SELECT against the engine.
    fn materialise_lateral_for_outer(
        &self,
        inner: &SelectStatement,
        outer_schema: &[ColumnSchema],
        outer_row: &Row<'static>,
    ) -> Result<Vec<Row<'static>>, EngineError> {
        let mut substituted = inner.clone();
        substitute_outer_columns_multi(&mut substituted, outer_row, outer_schema);
        let result = self.exec_bare_select_cancel(&substituted, CancelToken::none())?;
        match result {
            QueryResult::Rows { rows, .. } => Ok(rows),
            _ => Err(EngineError::Unsupported(
                "LATERAL subquery must be a SELECT (cannot be a write statement)".into(),
            )),
        }
    }

    /// v7.30.3 (mailrs round-26) — bounded execution for the backfill
    /// shape that walked prod into reclaim livelock:
    ///
    ///   SELECT … FROM big b JOIN small s ON b.k = s.k
    ///   WHERE … ORDER BY … LIMIT n
    ///
    /// The general join path materialises the FULL join+filter result
    /// (≈2× the table's fat columns on a fresh backfill scan) before
    /// LIMIT truncates to n rows. Here the primary streams row-by-row
    /// against a hash of the materialised peer, and accepted rows feed
    /// a keep = LIMIT+OFFSET bounded top-N heap — peak memory scales
    /// with the answer, not the table. Returns Ok(None) when the shape
    /// doesn't qualify; the caller falls through to the general path,
    /// which the byte budget guards.
    /// v7.34.5 (mailrs prod #5 / `content_worker` 250 k) — walker-
    /// driven sibling of `try_streamed_inner_join_topn`. When the
    /// outer ORDER BY is on an indexed primary column, drive the
    /// primary scan via the BTree iterator in the requested
    /// direction so rows arrive already in ORDER BY order; the join
    /// + WHERE filter + early-stop run unchanged afterwards, BUT
    /// the heap-based top-N (which still walks every primary row)
    /// becomes a plain `Vec` that breaks after `LIMIT + OFFSET`
    /// survivors. Mirrors the single-table `try_pk_walk_top_n`
    /// eligibility gates plus the `try_streamed_inner_join_topn`
    /// join-shape gates. Returns `None` on any miss → the legacy
    /// heap streamer + general path handle it.
    /// v7.37.x (docker-fair NOTEX attack) — short-circuit
    ///   SELECT COUNT(*) FROM A LEFT JOIN B ON B.k = A.fk WHERE B.k IS NULL
    /// (the v7.37.27 NOT-EXISTS pull-up output shape). Materialising
    /// every (outer, NULL-padded right) tuple just to count survivors
    /// is wasted work — build a `HashSet<i64>` of B's unique join
    /// values (B.k must be UNIQUE / PK on a single integer column), scan
    /// A's storage, and increment the counter on each miss. PG's Merge
    /// Anti-Join does the same shape over both PK indexes. Returns
    /// `None` on any eligibility miss; the general join + aggregate
    /// path handles non-matching shapes.
    pub(crate) fn try_count_star_left_anti_join_fast(
        &self,
        stmt: &SelectStatement,
        from: &FromClause,
    ) -> Result<Option<QueryResult>, EngineError> {
        use spg_sql::ast::{JoinKind, SelectItem};
        ANTI_JOIN_FAST_PATH_TRIED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        if stmt.distinct
            || stmt.limit_with_ties
            || stmt.group_by.is_some()
            || stmt.having.is_some()
            || !stmt.unions.is_empty()
            || !stmt.order_by.is_empty()
            || stmt.limit.is_some()
            || stmt.offset.is_some()
        {
            return Ok(None);
        }
        if from.joins.len() != 1 {
            return Ok(None);
        }
        let join = &from.joins[0];
        if !matches!(join.kind, JoinKind::Left) {
            return Ok(None);
        }
        // Gate: outer + inner must be plain catalog tables.
        let plain = |t: &spg_sql::ast::TableRef| {
            t.unnest_expr.is_none()
                && t.lateral_subquery.is_none()
                && t.as_of_segment.is_none()
                && t.generate_series_args.is_none()
        };
        if !plain(&from.primary) || !plain(&join.table) {
            return Ok(None);
        }
        let outer_alias = from
            .primary
            .alias
            .as_deref()
            .unwrap_or(from.primary.name.as_str());
        let inner_alias = join
            .table
            .alias
            .as_deref()
            .unwrap_or(join.table.name.as_str());
        // Items must be a single `COUNT(*)`.
        if stmt.items.len() != 1 {
            return Ok(None);
        }
        let SelectItem::Expr { expr, .. } = &stmt.items[0] else {
            return Ok(None);
        };
        let is_count_star = matches!(expr, Expr::FunctionCall { name, args }
            if name.eq_ignore_ascii_case("count_star") && args.is_empty());
        if !is_count_star {
            return Ok(None);
        }
        // ON clause: single equality on outer.col = inner.col (or
        // commuted). Capture (outer_col, inner_col).
        let Some(on) = join.on.as_ref() else {
            return Ok(None);
        };
        let Some((outer_col, inner_col)) = analyse_join_eq(on, outer_alias, inner_alias)? else {
            return Ok(None);
        };
        // WHERE clause: single `inner_alias.inner_col IS NULL` predicate
        // (canonical anti-join filter).
        let Some(where_expr) = stmt.where_.as_ref() else {
            return Ok(None);
        };
        if !is_inner_is_null(where_expr, inner_alias, &inner_col) {
            return Ok(None);
        }
        // Inner column must be UNIQUE / PK on a single integer column,
        // so the antiset built from B's rows is collision-free under
        // `HashSet<i64>`. Look the table up in the catalog.
        let catalog = self.active_catalog();
        let Some(inner_table) = catalog.get(join.table.name.as_str()) else {
            return Ok(None);
        };
        let inner_schema = inner_table.schema();
        let Some(inner_pos) = inner_schema
            .columns
            .iter()
            .position(|c| c.name.eq_ignore_ascii_case(&inner_col))
        else {
            return Ok(None);
        };
        let inner_ty = inner_schema.columns[inner_pos].ty;
        if !matches!(
            inner_ty,
            spg_storage::DataType::BigInt
                | spg_storage::DataType::Int
                | spg_storage::DataType::SmallInt
        ) {
            return Ok(None);
        }
        // The inner column must be a single-column PK so the antiset is
        // collision-free. (Single-column UNIQUE follows the same rule;
        // restricting Phase 1 to PK keeps the gate trivial.)
        if !inner_schema
            .uniqueness_constraints
            .iter()
            .any(|u| u.is_primary_key && u.columns.as_slice() == [inner_pos])
        {
            return Ok(None);
        }
        let Some(outer_table) = catalog.get(from.primary.name.as_str()) else {
            return Ok(None);
        };
        let outer_schema = outer_table.schema();
        let Some(outer_pos) = outer_schema
            .columns
            .iter()
            .position(|c| c.name.eq_ignore_ascii_case(&outer_col))
        else {
            return Ok(None);
        };
        let outer_ty = outer_schema.columns[outer_pos].ty;
        if !matches!(
            outer_ty,
            spg_storage::DataType::BigInt
                | spg_storage::DataType::Int
                | spg_storage::DataType::SmallInt
        ) {
            return Ok(None);
        }
        // Build the antiset.
        let read_int = |v: &Value| -> Option<i64> {
            match v {
                Value::BigInt(n) => Some(*n),
                Value::Int(n) => Some(i64::from(*n)),
                Value::SmallInt(n) => Some(i64::from(*n)),
                _ => None,
            }
        };
        // Phase C.3 step 2b — MVCC read gate for the anti-join count
        // fast path. Both loops iterate hot-tier rows (`.rows()`) by
        // physical index, so a row this snapshot cannot see must neither
        // seed the antiset nor be counted. No-op today: every hot header
        // is frozen/committed-alive.
        let scan_snapshot = self.current_snapshot();
        let mut antiset: hashbrown::HashSet<i64> =
            hashbrown::HashSet::with_capacity(inner_table.row_count());
        for (i, row) in inner_table.rows().iter().enumerate() {
            if !inner_table.is_row_visible(i, &scan_snapshot) {
                continue;
            }
            if let Some(v) = row.values.get(inner_pos)
                && let Some(k) = read_int(v)
            {
                antiset.insert(k);
            }
        }
        // Walk outer; count rows whose key isn't in the set OR whose key
        // is NULL (a NULL outer key has no join match either way).
        let mut count: i64 = 0;
        for (i, row) in outer_table.rows().iter().enumerate() {
            if !outer_table.is_row_visible(i, &scan_snapshot) {
                continue;
            }
            match row.values.get(outer_pos) {
                Some(v) => match read_int(v) {
                    Some(k) => {
                        if !antiset.contains(&k) {
                            count += 1;
                        }
                    }
                    None => count += 1,
                },
                None => count += 1,
            }
        }
        let columns = alloc::vec![ColumnSchema::new(
            "count".to_string(),
            spg_storage::DataType::BigInt,
            false,
        )];
        let rows = alloc::vec![Row::new(alloc::vec![Value::BigInt(count)])];
        let _ = outer_alias;
        let _ = (outer_col, inner_col);
        ANTI_JOIN_FAST_PATH_FIRED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        Ok(Some(QueryResult::Rows { columns, rows }))
    }

    pub(crate) fn try_streamed_inner_join_walk_topn(
        &self,
        stmt: &SelectStatement,
        from: &FromClause,
        cancel: CancelToken<'_>,
    ) -> Result<Option<QueryResult>, EngineError> {
        let Some(limit) = stmt.limit_literal() else {
            return Ok(None);
        };
        if stmt.offset.is_some() && stmt.offset_literal().is_none() {
            return Ok(None);
        }
        if stmt.distinct
            || stmt.limit_with_ties
            || stmt.group_by.is_some()
            || stmt.having.is_some()
            || aggregate::uses_aggregate(stmt)
        {
            return Ok(None);
        }
        if from.joins.len() != 1 {
            return Ok(None);
        }
        let j = &from.joins[0];
        if !matches!(j.kind, JoinKind::Inner) {
            return Ok(None);
        }
        let plain = |t: &TableRef| {
            t.unnest_expr.is_none() && t.lateral_subquery.is_none() && t.as_of_segment.is_none()
        };
        if !plain(&from.primary) || !plain(&j.table) {
            return Ok(None);
        }
        let Some(on_expr) = j.on.as_ref() else {
            return Ok(None);
        };
        let Some(primary_table) = self.active_catalog().get(&from.primary.name) else {
            return Ok(None);
        };
        if self.active_catalog().get(&j.table.name).is_none() {
            return Ok(None);
        }
        let primary_alias = from
            .primary
            .alias
            .as_deref()
            .unwrap_or(from.primary.name.as_str())
            .to_string();
        // Walker eligibility — single-key ORDER BY on a btree-indexed
        // primary column.
        if stmt.order_by.len() != 1 {
            return Ok(None);
        }
        let order = &stmt.order_by[0];
        let Expr::Column(order_col) = &order.expr else {
            return Ok(None);
        };
        if let Some(q) = &order_col.qualifier
            && !q.eq_ignore_ascii_case(&primary_alias)
        {
            return Ok(None);
        }
        let primary_cols = primary_table.schema().columns.clone();
        let Some(order_col_pos) = primary_cols
            .iter()
            .position(|c| c.name.eq_ignore_ascii_case(&order_col.name))
        else {
            return Ok(None);
        };
        let Some(order_index) = primary_table.index_on(order_col_pos) else {
            return Ok(None);
        };
        if !matches!(order_index.kind, spg_storage::IndexKind::BTree(_)) {
            return Ok(None);
        }
        // Peer side: same materialise + prune as the heap streamer.
        let peer_alias = j
            .table
            .alias
            .as_deref()
            .unwrap_or(j.table.name.as_str())
            .to_string();
        let mut needed = alloc::collections::BTreeSet::new();
        let prunable = collect_qualified_refs(stmt, &mut needed).is_some();
        let mut budget = ByteBudget::new(self.max_query_bytes);
        let (mut peer_rows, peer_cols) = self.materialise_table_ref_filtered(&j.table, &[])?;
        if prunable {
            Self::null_out_unreferenced(&mut peer_rows, &peer_cols, &peer_alias, &needed);
        }
        budget.charge(approx_rows_bytes(&peer_rows))?;
        let mut combined_schema: Vec<ColumnSchema> = Vec::new();
        for col in &primary_cols {
            combined_schema.push(ColumnSchema::new(
                alloc::format!("{primary_alias}.{}", col.name),
                col.ty,
                col.nullable,
            ));
        }
        for col in &peer_cols {
            combined_schema.push(ColumnSchema::new(
                alloc::format!("{peer_alias}.{}", col.name),
                col.ty,
                col.nullable,
            ));
        }
        // v7.39 (read01 round 53) — the join's EvalContext must carry the
        // catalog. Without it a `::regclass` / enum / composite cast inside a
        // joined WHERE or ON falls back to plain text, so the canonical
        // `pg_class JOIN pg_index … WHERE indrelid = 't'::regclass` shape
        // errored on "comparison between BigInt and Text" — while the very
        // same predicate worked on a single-table SELECT (whose ctx does carry
        // the catalog). Same root as round 49's unnest(enum_range(…)).
        // v7.39 (round 525) — and the SESSION, for the same reason as the
        // catalog above: a join's WHERE is the same predicate a
        // single-table SELECT would carry, and `WHERE t =
        // current_setting('app.tenant')` failed on the joined shape while
        // working on the unjoined one.
        let join_sess = self.dml_session();
        let ctx = EvalContext::new(&combined_schema, None)
            .with_catalog(self.active_catalog())
            .with_session(&join_sess);
        let left_arity = primary_cols.len();
        let mut eq_pairs: Vec<(usize, usize)> = Vec::new();
        let mut residual: Vec<&Expr> = Vec::new();
        for sub in reorder::split_and_conjunctions(on_expr) {
            let mut matched = None;
            if let Expr::Binary {
                lhs,
                op: spg_sql::ast::BinOp::Eq,
                rhs,
            } = sub
                && let (Expr::Column(a), Expr::Column(b)) = (lhs.as_ref(), rhs.as_ref())
            {
                let left_slice = &combined_schema[..left_arity];
                if let (Some(l), Some(r)) = (
                    Self::composite_col_pos(left_slice, a),
                    Self::peer_col_pos(&peer_alias, &peer_cols, b),
                ) {
                    matched = Some((l, r));
                } else if let (Some(l), Some(r)) = (
                    Self::composite_col_pos(left_slice, b),
                    Self::peer_col_pos(&peer_alias, &peer_cols, a),
                ) {
                    matched = Some((l, r));
                }
            }
            match matched {
                Some(pair) => eq_pairs.push(pair),
                None => residual.push(sub),
            }
        }
        if eq_pairs.is_empty() {
            return Ok(None);
        }
        // Hash the peer on the equality key (same as the heap streamer).
        let mut htable: hashbrown::HashMap<String, Vec<usize>> =
            hashbrown::HashMap::with_capacity(peer_rows.len());
        let mut keybuf: Vec<Value<'static>> = Vec::with_capacity(eq_pairs.len());
        'build: for (ri, right) in peer_rows.iter().enumerate() {
            keybuf.clear();
            for (_, rpos) in &eq_pairs {
                let v = right.values.get(*rpos).cloned().unwrap_or(Value::Null);
                if matches!(v, Value::Null) {
                    continue 'build;
                }
                keybuf.push(v);
            }
            htable
                .entry(aggregate::encode_key(&keybuf))
                .or_default()
                .push(ri);
        }
        let keep_mask: Vec<bool> = primary_cols
            .iter()
            .map(|c| !prunable || needed.contains(&(primary_alias.clone(), c.name.clone())))
            .collect();
        let keep = (limit as usize).saturating_add(stmt.offset_literal().map_or(0, |o| o as usize));
        let mut where_memo = memoize::MemoizeCache::default();
        let mut plain_sink: Vec<Row<'static>> = Vec::with_capacity(keep.min(1024));
        // v7.37.6 (mailrs content_worker — Attack #2 from
        // `v7.37.5-content-worker-prod-decomposition.md`): pre-split
        // the WHERE predicate into conjuncts that reference only the
        // outer (`primary_alias`) and conjuncts that touch the peer
        // alias (mixed). Outer-only conjuncts can be evaluated against
        // `left` BEFORE we materialise `combined_vals`, which lets the
        // 25 k-iter content_worker hot path skip the 12-cell per-row
        // clone when the InList probe (`m.id NOT IN`) misses — which it
        // does on > 99 % of rows in the prod snapshot.
        //
        // We also split the ON residual the same way so a `mb.foo = …`
        // predicate that's been mis-folded into the residual still goes
        // through the slow combined-row path, and an `m.foo = …` one
        // gates before the materialise.
        //
        // Implementation notes:
        // - We allocate the split once per query (the walker iterates,
        //   the split does not).
        // - Outer-only conjuncts evaluate against
        //   `&combined_schema[..left_arity]` paired with `left`, so the
        //   column resolver indices match the way they would on the
        //   combined row (positions 0..left_arity are identical).
        // - The full WHERE still re-runs on the post-materialise path
        //   only for the conjuncts the split could not classify as
        //   outer-only (this keeps the semantics identical even when an
        //   unknown / subquery node is present; `expr_references_alias`
        //   is conservative).
        let outer_schema: &[ColumnSchema] = &combined_schema[..left_arity];
        let outer_ctx = EvalContext::new(outer_schema, None).with_catalog(self.active_catalog());
        let where_conjuncts: Vec<&Expr> = stmt
            .where_
            .as_ref()
            .map(|w| reorder::split_and_conjunctions(w))
            .unwrap_or_default();
        let (where_outer_only, where_mixed): (Vec<&Expr>, Vec<&Expr>) =
            where_conjuncts.iter().copied().partition(|e| {
                crate::joinfold::expr_references_alias(e, &primary_alias)
                    && !crate::joinfold::expr_references_any_other_alias(e, &primary_alias)
            });
        let (residual_outer_only, residual_mixed): (Vec<&Expr>, Vec<&Expr>) =
            residual.iter().copied().partition(|e| {
                crate::joinfold::expr_references_alias(e, &primary_alias)
                    && !crate::joinfold::expr_references_any_other_alias(e, &primary_alias)
            });
        let mut outer_memo = memoize::MemoizeCache::default();
        // Walker drive: walk primary via btree index in ORDER BY
        // direction. Rows arrive already sorted; plain_sink + early
        // stop replaces the heap.
        let walker: alloc::boxed::Box<
            dyn Iterator<Item = (&spg_storage::IndexKey, &Vec<spg_storage::RowLocator>)>,
        > = if order.desc {
            alloc::boxed::Box::new(order_index.iter_desc())
        } else {
            alloc::boxed::Box::new(order_index.iter_asc())
        };
        let primary_table_name = primary_table.schema().name.clone();
        // Phase C.3 step 2b — MVCC read gate for the streamed-join
        // walker's primary. Snapshot computed once; a hot primary row
        // this snapshot cannot see is skipped so a dead/old version never
        // drives a join tuple. Cold locators are frozen = always visible.
        // No-op today: every hot header is frozen/committed-alive.
        let scan_snapshot = self.current_snapshot();
        'walk: for (key, locators) in walker {
            cancel.check()?;
            for loc in locators {
                // v7.34.6 (mailrs prod #6) — cold-tier dispatch on the
                // walker. Pre-v7.34.6 bailed the whole walker on the
                // first cold locator, which is exactly the prod-803MB
                // shape: messages at scale has older rows promoted to
                // cold segments, so the ORDER BY id DESC walk hits a
                // cold locator on the very first batch and the entire
                // plan falls back to the 82ms NOT-IN scan-and-sort.
                // `Catalog::resolve_cold_locator` reads one cold
                // segment page + decodes the dense row body, which
                // ports the walker's early-stop across the tier
                // boundary at ~µs per row.
                let left_cow: Cow<'_, Row> = match *loc {
                    spg_storage::RowLocator::Hot(i) => {
                        if !primary_table.is_row_visible(i, &scan_snapshot) {
                            continue;
                        }
                        match primary_table.rows().get(i) {
                            Some(r) => Cow::Borrowed(r),
                            None => continue,
                        }
                    }
                    spg_storage::RowLocator::Cold { segment_id, .. } => {
                        match self.active_catalog().resolve_cold_locator(
                            &primary_table_name,
                            segment_id,
                            key,
                        ) {
                            Some(r) => Cow::Owned(r),
                            None => continue,
                        }
                    }
                };
                let left: &Row<'static> = left_cow.as_ref();
                keybuf.clear();
                let mut left_has_null = false;
                for (lpos, _) in &eq_pairs {
                    let v = left.values.get(*lpos).cloned().unwrap_or(Value::Null);
                    if matches!(v, Value::Null) {
                        left_has_null = true;
                        break;
                    }
                    keybuf.push(v);
                }
                if left_has_null {
                    continue;
                }
                let Some(cands) = htable.get(&aggregate::encode_key(&keybuf)) else {
                    continue;
                };
                // v7.37.6 — gate the outer-only WHERE conjuncts +
                // outer-only ON residual on `left` before we ever clone
                // into `combined_vals`. content_worker's `m.size > 0
                // AND m.id NOT IN (…25 k…)` is outer-only on `m.*`; if
                // the InList probe misses (>99 %), we skip the 12-cell
                // clone + extend + Row::new + budget charge for every
                // peer candidate this outer row hashed to.
                let mut outer_ok = true;
                for r in &residual_outer_only {
                    let cond = self.eval_expr_with_correlated(r, left, &outer_ctx, cancel, None)?;
                    if !crate::eval::predicate_is_true(&cond, "JOIN/ON", ctx.mysql_dialect)? {
                        outer_ok = false;
                        break;
                    }
                }
                if !outer_ok {
                    continue;
                }
                for w in &where_outer_only {
                    let cond = self.eval_expr_with_correlated(
                        w,
                        left,
                        &outer_ctx,
                        cancel,
                        Some(&mut outer_memo),
                    )?;
                    if !crate::eval::predicate_is_true(&cond, "JOIN/ON", ctx.mysql_dialect)? {
                        outer_ok = false;
                        break;
                    }
                }
                if !outer_ok {
                    continue;
                }
                for &ri in cands {
                    let right = &peer_rows[ri];
                    let mut combined_vals: Vec<Value<'static>> =
                        Vec::with_capacity(left_arity + peer_cols.len());
                    for (i, v) in left.values.iter().enumerate() {
                        combined_vals.push(if keep_mask.get(i).copied().unwrap_or(true) {
                            v.clone()
                        } else {
                            Value::Null
                        });
                    }
                    combined_vals.extend(right.values.iter().cloned());
                    let combined = Row::new(combined_vals);
                    let mut ok = true;
                    for r in &residual_mixed {
                        let cond =
                            self.eval_expr_with_correlated(r, &combined, &ctx, cancel, None)?;
                        if !crate::eval::predicate_is_true(&cond, "JOIN/ON", ctx.mysql_dialect)? {
                            ok = false;
                            break;
                        }
                    }
                    if !ok {
                        continue;
                    }
                    for w in &where_mixed {
                        let cond = self.eval_expr_with_correlated(
                            w,
                            &combined,
                            &ctx,
                            cancel,
                            Some(&mut where_memo),
                        )?;
                        if !crate::eval::predicate_is_true(&cond, "JOIN/ON", ctx.mysql_dialect)? {
                            ok = false;
                            break;
                        }
                    }
                    if !ok {
                        continue;
                    }
                    budget.charge(approx_row_bytes(&combined))?;
                    plain_sink.push(combined);
                    if plain_sink.len() >= keep {
                        break 'walk;
                    }
                }
            }
        }
        // Already in ORDER BY order from the walk.
        let mut output = plain_sink;
        apply_offset_and_limit(&mut output, stmt.offset_literal(), stmt.limit_literal());
        let projection = build_projection(&stmt.items, &combined_schema, "", self.backslash_escapes)?;
        let mut proj_memo = memoize::MemoizeCache::default();
        let mut rows: Vec<Row<'static>> = Vec::with_capacity(output.len());
        for row in &output {
            let mut values = Vec::with_capacity(projection.len());
            for p in &projection {
                values.push(self.eval_expr_with_correlated(
                    &p.expr,
                    row,
                    &ctx,
                    cancel,
                    Some(&mut proj_memo),
                )?);
            }
            rows.push(Row::new(values));
        }
        let columns: Vec<ColumnSchema> = projection
            .into_iter()
            .map(|p| ColumnSchema::new(p.output_name, p.ty, p.nullable))
            .collect();
        Ok(Some(QueryResult::Rows { columns, rows }))
    }

    pub(crate) fn try_streamed_inner_join_topn(
        &self,
        stmt: &SelectStatement,
        from: &FromClause,
        cancel: CancelToken<'_>,
    ) -> Result<Option<QueryResult>, EngineError> {
        // Shape gate — any bail lands on the general path.
        let Some(limit) = stmt.limit_literal() else {
            return Ok(None);
        };
        if stmt.offset.is_some() && stmt.offset_literal().is_none() {
            return Ok(None);
        }
        if stmt.distinct
            || stmt.group_by.is_some()
            || stmt.having.is_some()
            || aggregate::uses_aggregate(stmt)
        {
            return Ok(None);
        }
        if from.joins.len() != 1 {
            return Ok(None);
        }
        let j = &from.joins[0];
        if !matches!(j.kind, JoinKind::Inner) {
            return Ok(None);
        }
        let plain = |t: &TableRef| {
            t.unnest_expr.is_none() && t.lateral_subquery.is_none() && t.as_of_segment.is_none()
        };
        if !plain(&from.primary) || !plain(&j.table) {
            return Ok(None);
        }
        let Some(on_expr) = j.on.as_ref() else {
            return Ok(None);
        };
        // Plain catalog tables only — views / virtual tables keep the
        // general path's materialise_table_ref fallback.
        let Some(primary_table) = self.active_catalog().get(&from.primary.name) else {
            return Ok(None);
        };
        if self.active_catalog().get(&j.table.name).is_none() {
            return Ok(None);
        }
        let primary_alias = from
            .primary
            .alias
            .as_deref()
            .unwrap_or(from.primary.name.as_str())
            .to_string();
        let peer_alias = j
            .table
            .alias
            .as_deref()
            .unwrap_or(j.table.name.as_str())
            .to_string();
        let mut needed = alloc::collections::BTreeSet::new();
        let prunable = collect_qualified_refs(stmt, &mut needed).is_some();
        // Peer side: materialise + prune exactly like the general
        // path; the budget still guards a degenerately fat peer.
        let mut budget = ByteBudget::new(self.max_query_bytes);
        let (mut peer_rows, peer_cols) = self.materialise_table_ref_filtered(&j.table, &[])?;
        if prunable {
            Self::null_out_unreferenced(&mut peer_rows, &peer_cols, &peer_alias, &needed);
        }
        budget.charge(approx_rows_bytes(&peer_rows))?;
        let primary_cols = primary_table.schema().columns.clone();
        let mut combined_schema: Vec<ColumnSchema> = Vec::new();
        for col in &primary_cols {
            combined_schema.push(ColumnSchema::new(
                alloc::format!("{primary_alias}.{}", col.name),
                col.ty,
                col.nullable,
            ));
        }
        for col in &peer_cols {
            combined_schema.push(ColumnSchema::new(
                alloc::format!("{peer_alias}.{}", col.name),
                col.ty,
                col.nullable,
            ));
        }
        // v7.39 (read01 round 53) — the join's EvalContext must carry the
        // catalog. Without it a `::regclass` / enum / composite cast inside a
        // joined WHERE or ON falls back to plain text, so the canonical
        // `pg_class JOIN pg_index … WHERE indrelid = 't'::regclass` shape
        // errored on "comparison between BigInt and Text" — while the very
        // same predicate worked on a single-table SELECT (whose ctx does carry
        // the catalog). Same root as round 49's unnest(enum_range(…)).
        // v7.39 (round 525) — and the SESSION, for the same reason as the
        // catalog above: a join's WHERE is the same predicate a
        // single-table SELECT would carry, and `WHERE t =
        // current_setting('app.tenant')` failed on the joined shape while
        // working on the unjoined one.
        let join_sess = self.dml_session();
        let ctx = EvalContext::new(&combined_schema, None)
            .with_catalog(self.active_catalog())
            .with_session(&join_sess);
        // Hash-joinable left = right equality pairs from ON; anything
        // else stays as a residual conjunct on the candidate row.
        let left_arity = primary_cols.len();
        let mut eq_pairs: Vec<(usize, usize)> = Vec::new();
        let mut residual: Vec<&Expr> = Vec::new();
        for sub in reorder::split_and_conjunctions(on_expr) {
            let mut matched = None;
            if let Expr::Binary {
                lhs,
                op: spg_sql::ast::BinOp::Eq,
                rhs,
            } = sub
                && let (Expr::Column(a), Expr::Column(b)) = (lhs.as_ref(), rhs.as_ref())
            {
                let left_slice = &combined_schema[..left_arity];
                if let (Some(l), Some(r)) = (
                    Self::composite_col_pos(left_slice, a),
                    Self::peer_col_pos(&peer_alias, &peer_cols, b),
                ) {
                    matched = Some((l, r));
                } else if let (Some(l), Some(r)) = (
                    Self::composite_col_pos(left_slice, b),
                    Self::peer_col_pos(&peer_alias, &peer_cols, a),
                ) {
                    matched = Some((l, r));
                }
            }
            match matched {
                Some(pair) => eq_pairs.push(pair),
                None => residual.push(sub),
            }
        }
        if eq_pairs.is_empty() {
            return Ok(None); // nested-loop shapes stay on the general path
        }
        // Hash the peer on the equality key (NULL keys never match).
        let mut htable: hashbrown::HashMap<String, Vec<usize>> =
            hashbrown::HashMap::with_capacity(peer_rows.len());
        let mut keybuf: Vec<Value<'static>> = Vec::with_capacity(eq_pairs.len());
        'build: for (ri, right) in peer_rows.iter().enumerate() {
            keybuf.clear();
            for (_, rpos) in &eq_pairs {
                let v = right.values.get(*rpos).cloned().unwrap_or(Value::Null);
                if matches!(v, Value::Null) {
                    continue 'build;
                }
                keybuf.push(v);
            }
            htable
                .entry(aggregate::encode_key(&keybuf))
                .or_default()
                .push(ri);
        }
        // Streamed twin of null_out_unreferenced: clone only the
        // referenced primary columns into each candidate row.
        let keep_mask: Vec<bool> = primary_cols
            .iter()
            .map(|c| !prunable || needed.contains(&(primary_alias.clone(), c.name.clone())))
            .collect();
        let keep = (limit as usize).saturating_add(stmt.offset_literal().map_or(0, |o| o as usize));
        let descs: alloc::rc::Rc<[bool]> = stmt
            .order_by
            .iter()
            .map(|o| o.desc)
            .collect::<Vec<bool>>()
            .into();
        let mut where_memo = memoize::MemoizeCache::default();
        let mut heap: alloc::collections::BinaryHeap<TopNEntry> =
            alloc::collections::BinaryHeap::new();
        let mut plain_sink: Vec<Row<'static>> = Vec::new();
        let mut seq: u64 = 0;
        // v7.36 (cold-tier coverage) — extend the primary scan with
        // the cold-tier rows so `ORDER BY <non-indexed> LIMIT N`
        // doesn't lose half a freezer-promoted table when the walker
        // shape isn't a match. Hot rows borrow from `PersistentVec`;
        // cold rows are pre-materialised once and yielded in order.
        let primary_cold = self.iter_cold_rows_of_table(primary_table);
        // v7.37.15 Phase B — visibility gate the primary join scan.
        // The cold tier still iterates directly because cold rows have
        // no per-row header (they're frozen segments — equivalent to
        // RowHeader::frozen() for visibility purposes). Phase D wires
        // per-segment all-visible bitmaps so cold scans skip the
        // visibility check entirely.
        let snap = self.current_snapshot();
        'scan: for left in primary_table
            .scan_visible(&snap)
            .map(|(_, r)| r)
            .chain(primary_cold.iter())
        {
            cancel.check()?;
            if keep == 0 {
                break 'scan;
            }
            keybuf.clear();
            let mut left_has_null = false;
            for (lpos, _) in &eq_pairs {
                let v = left.values.get(*lpos).cloned().unwrap_or(Value::Null);
                if matches!(v, Value::Null) {
                    left_has_null = true;
                    break;
                }
                keybuf.push(v);
            }
            if left_has_null {
                continue;
            }
            let Some(cands) = htable.get(&aggregate::encode_key(&keybuf)) else {
                continue;
            };
            for &ri in cands {
                let right = &peer_rows[ri];
                let mut combined_vals: Vec<Value<'static>> =
                    Vec::with_capacity(left_arity + peer_cols.len());
                for (i, v) in left.values.iter().enumerate() {
                    combined_vals.push(if keep_mask.get(i).copied().unwrap_or(true) {
                        v.clone()
                    } else {
                        Value::Null
                    });
                }
                combined_vals.extend(right.values.iter().cloned());
                let combined = Row::new(combined_vals);
                let mut ok = true;
                for r in &residual {
                    let cond = self.eval_expr_with_correlated(r, &combined, &ctx, cancel, None)?;
                    if !crate::eval::predicate_is_true(&cond, "JOIN/ON", ctx.mysql_dialect)? {
                        ok = false;
                        break;
                    }
                }
                if !ok {
                    continue;
                }
                if let Some(w) = stmt.where_.as_ref() {
                    let cond = self.eval_expr_with_correlated(
                        w,
                        &combined,
                        &ctx,
                        cancel,
                        Some(&mut where_memo),
                    )?;
                    if !crate::eval::predicate_is_true(&cond, "JOIN/ON", ctx.mysql_dialect)? {
                        continue;
                    }
                }
                if stmt.order_by.is_empty() {
                    budget.charge(approx_row_bytes(&combined))?;
                    plain_sink.push(combined);
                    if plain_sink.len() >= keep {
                        break 'scan;
                    }
                } else {
                    let keys = build_order_keys(&stmt.order_by, &combined, &ctx)?;
                    let entry = TopNEntry {
                        keys,
                        descs: alloc::rc::Rc::clone(&descs),
                        seq,
                        row: combined,
                    };
                    seq += 1;
                    if heap.len() < keep {
                        budget.charge(approx_row_bytes(&entry.row))?;
                        heap.push(entry);
                    } else if let Some(top) = heap.peek()
                        && entry < *top
                    {
                        if let Some(evicted) = heap.pop() {
                            budget.release(approx_row_bytes(&evicted.row));
                        }
                        budget.charge(approx_row_bytes(&entry.row))?;
                        heap.push(entry);
                    }
                }
            }
        }
        let mut output: Vec<Row<'static>> = if stmt.order_by.is_empty() {
            plain_sink
        } else {
            heap.into_sorted_vec().into_iter().map(|e| e.row).collect()
        };
        apply_offset_and_limit(&mut output, stmt.offset_literal(), stmt.limit_literal());
        let projection = build_projection(&stmt.items, &combined_schema, "", self.backslash_escapes)?;
        let mut proj_memo = memoize::MemoizeCache::default();
        let mut rows: Vec<Row<'static>> = Vec::with_capacity(output.len());
        for row in &output {
            let mut values = Vec::with_capacity(projection.len());
            for p in &projection {
                values.push(self.eval_expr_with_correlated(
                    &p.expr,
                    row,
                    &ctx,
                    cancel,
                    Some(&mut proj_memo),
                )?);
            }
            rows.push(Row::new(values));
        }
        let columns: Vec<ColumnSchema> = projection
            .into_iter()
            .map(|p| ColumnSchema::new(p.output_name, p.ty, p.nullable))
            .collect();
        Ok(Some(QueryResult::Rows { columns, rows }))
    }
}

/// v7.17.0 Phase 3.P0-41 — synthesise a column name for a LATERAL
/// projection item that has no explicit alias. PG names anonymous
/// projection items by the function call's name or by `column<i>`.
/// SPG mirrors the latter (lower-overhead than walking arbitrary
/// Expr shapes) so the probe-schema fallback path produces stable
/// names for the lateral peer's columns.
pub(crate) fn synth_lateral_col_name(expr: &Expr, idx: usize) -> String {
    match expr {
        // Bare column reference — use the column's own name.
        Expr::Column(c) => c.name.clone(),
        // Function call — use the function name (PG canonical:
        // `count` / `max` / `lower` …).
        Expr::FunctionCall { name, .. } => name.clone(),
        // Cast — drill into the inner expression.
        Expr::Cast { expr: inner, .. } => synth_lateral_col_name(inner, idx),
        // Everything else falls back to PG's `column<N>` placeholder.
        _ => alloc::format!("column{}", idx + 1),
    }
}

/// v7.17.0 Phase 3.P0-41 — substitute every `<alias>.<col>` Expr
/// reference whose `<alias>.<col>` exists in the outer composite
/// schema with the matching value from the outer row. Walks the
/// entire SELECT body (items, WHERE, GROUP BY, HAVING, ORDER BY,
/// UNION peers) so any depth of outer reference inside the
/// LATERAL subquery resolves before execution.
/// True when `e` is a compile-time constant — no column ref, function call,
/// subquery, or other outer-touching construct. Used to recognise a bare
/// `(VALUES …)` derived table (whose rows are pure literals) so it can be
/// eager-materialised as a join peer rather than forced through the per-left-row
/// lateral path (see D.19).
fn expr_is_constant(e: &Expr) -> bool {
    match e {
        Expr::Literal(_) | Expr::Placeholder(_) => true,
        Expr::Unary { expr, .. } | Expr::Cast { expr, .. } => expr_is_constant(expr),
        Expr::Binary { lhs, rhs, .. } => expr_is_constant(lhs) && expr_is_constant(rhs),
        _ => false,
    }
}

/// True when `s` is a constant `(VALUES …)`-shaped derived table: the head and
/// every UNION-ALL peer has no FROM clause and projects only constant
/// expressions. Such a table references nothing outer, so it is safe to
/// materialise once and cross-join — unlike a correlated lateral (e.g.
/// `generate_series(1, outer.col)`), which must stay per-left-row.
/// v7.39 (round 572) — is this derived table a plain SELECT over stored
/// tables, with nothing set-returning in its FROM?
///
/// `select_is_correlated` answers about columns in the projection, the
/// WHERE and the nested subqueries. It does NOT see an outer reference
/// carried in a set-returning function's ARGUMENTS — `LATERAL
/// generate_series(1, lo.n)` and `LATERAL unnest(t.arr)` parse into a
/// synthesised SELECT whose correlation lives in `generate_series_args`
/// / `unnest_expr`, and it reports those as uncorrelated. Fifteen
/// lateral e2e tests said so the moment the gate widened.
///
/// So the eager path asks this first: every FROM item must be a named
/// stored table. An SRF anywhere keeps the per-left-row evaluation it
/// needs.
fn derived_is_plain_table_select(s: &SelectStatement, cat: &crate::Catalog) -> bool {
    let Some(from) = &s.from else {
        return false;
    };
    let plain = |t: &spg_sql::ast::TableRef| {
        t.unnest_expr.is_none()
            && t.generate_series_args.is_none()
            && t.lateral_subquery.is_none()
            // A set-returning FUNCTION reads as a named FROM item —
            // `LATERAL f(t.col)`, `jsonb_each_text(t.j)`, `json_table(…)`
            // all put the function's name here and their correlation in
            // the arguments. Resolving the name against the catalog is
            // what tells a stored table from one of those.
            && cat.get(&t.name).is_some()
    };
    plain(&from.primary) && from.joins.iter().all(|j| plain(&j.table))
}

fn is_constant_values_derived(s: &SelectStatement) -> bool {
    use spg_sql::ast::SelectItem;
    let peer_ok = |p: &SelectStatement| -> bool {
        p.from.is_none()
            && p.where_.is_none()
            && p.having.is_none()
            && p.items.iter().all(|it| match it {
                SelectItem::Expr { expr, .. } => expr_is_constant(expr),
                SelectItem::Wildcard | SelectItem::QualifiedWildcard(_) => false,
            })
    };
    peer_ok(s) && s.unions.iter().all(|(_, peer)| peer_ok(peer))
}

pub(crate) fn substitute_outer_columns_multi(
    stmt: &mut SelectStatement,
    outer_row: &Row<'static>,
    outer_schema: &[ColumnSchema],
) {
    substitute_outer_in_select(stmt, outer_row, outer_schema);
}

/// v4.23: walk every Expr in `stmt` and replace each Column ref
/// that targets the outer scope (qualifier matches the outer
/// table alias) with a Literal carrying the outer row's value.
/// Conservative: only qualified refs are substituted, so the user
/// must write `outer_alias.col` to reference an outer column. This
/// matches PG's lexical scoping for correlated subqueries and
/// avoids accidentally rebinding inner columns of the same name.
fn substitute_outer_in_select(
    stmt: &mut SelectStatement,
    outer_row: &Row<'static>,
    outer_schema: &[ColumnSchema],
) {
    // A FROM-less SELECT (`LATERAL (SELECT <outer col> …)`) has no inner
    // scope of its own, so an unqualified column in its projection /
    // predicates must resolve to the outer row. A SELECT with a FROM
    // keeps the conservative qualified-only rule: bare names resolve
    // against its own tables first (PG lexical scoping).
    let bare = stmt.from.is_none();
    for item in &mut stmt.items {
        if let SelectItem::Expr { expr, .. } = item {
            substitute_outer_in_expr(expr, outer_row, outer_schema, bare);
        }
    }
    // v7.37.43-T4.5 — walk FROM-side SRF argument expressions
    // (`unnest(<expr>)` / `generate_series(<args>)` /
    // `jsonb_each_text(<expr>)`) so a LATERAL SRF with an outer-
    // column reference gets the reference substituted before
    // per-row execution.
    if let Some(from) = &mut stmt.from {
        substitute_outer_in_table_ref(&mut from.primary, outer_row, outer_schema);
        for j in &mut from.joins {
            substitute_outer_in_table_ref(&mut j.table, outer_row, outer_schema);
            if let Some(on) = &mut j.on {
                substitute_outer_in_expr(on, outer_row, outer_schema, bare);
            }
        }
    }
    if let Some(w) = &mut stmt.where_ {
        substitute_outer_in_expr(w, outer_row, outer_schema, bare);
    }
    if let Some(gs) = &mut stmt.group_by {
        for g in gs {
            substitute_outer_in_expr(g, outer_row, outer_schema, bare);
        }
    }
    if let Some(h) = &mut stmt.having {
        substitute_outer_in_expr(h, outer_row, outer_schema, bare);
    }
    for o in &mut stmt.order_by {
        substitute_outer_in_expr(&mut o.expr, outer_row, outer_schema, bare);
    }
    for (_, peer) in &mut stmt.unions {
        substitute_outer_in_select(peer, outer_row, outer_schema);
    }
}

fn substitute_outer_in_table_ref(
    t: &mut spg_sql::ast::TableRef,
    outer_row: &Row<'static>,
    outer_schema: &[ColumnSchema],
) {
    // A set-returning FROM item's argument is evaluated with no inner
    // scope, so its unqualified column refs are outer refs
    // (`LATERAL unnest(<outer array col>)`).
    if let Some((_, arg)) = t.jsonb_each_text_arg.as_mut() {
        substitute_outer_in_expr(arg, outer_row, outer_schema, true);
    }
    if let Some(arg) = t.unnest_expr.as_deref_mut() {
        substitute_outer_in_expr(arg, outer_row, outer_schema, true);
    }
    // v7.39 (read01 round 69) — a user function on a JOIN's right side
    // (`t JOIN LATERAL dbl(t.id) AS d ON true`): its ARGUMENTS reference the
    // outer row, so they take the substitution too. Without this the call would
    // see an unresolved column and the correlation would silently not happen.
    if let Some(call) = t.table_fn_call.as_deref_mut() {
        for a in call.1.iter_mut() {
            substitute_outer_in_expr(a, outer_row, outer_schema, true);
        }
    }
    if let Some(args) = t.generate_series_args.as_mut() {
        for a in args.iter_mut() {
            substitute_outer_in_expr(a, outer_row, outer_schema, true);
        }
    }
    if let Some(inner) = t.lateral_subquery.as_deref_mut() {
        substitute_outer_in_select(inner, outer_row, outer_schema);
    }
    // v7.39 (round 205, JSON_TABLE) — the document expr (and PASSING
    // values) are evaluated with no inner scope, so their unqualified
    // / outer-qualified column refs are outer refs (implicit LATERAL:
    // `t, JSON_TABLE(t.arr, …)`).
    if let Some(jt) = t.json_table.as_deref_mut() {
        substitute_outer_in_expr(&mut jt.doc, outer_row, outer_schema, true);
        for (_, e) in jt.passing.iter_mut() {
            substitute_outer_in_expr(e, outer_row, outer_schema, true);
        }
    }
}

/// Index of the outer column a reference targets, or `None`. Qualified
/// refs (`outer_alias.col`) match the composite outer-schema name. When
/// `bare_ok` — i.e. the expression is evaluated with no inner FROM scope
/// of its own (a FROM-less LATERAL subquery, or a set-returning FROM
/// item's argument) — an *unqualified* ref also resolves to the outer
/// scope, matching the bare column against the last segment of each
/// outer name, but only when that match is unique (an ambiguous bare ref
/// is left for the normal resolver to reject, as PG does).
fn outer_col_index(
    outer_schema: &[ColumnSchema],
    qualifier: Option<&str>,
    name: &str,
    bare_ok: bool,
) -> Option<usize> {
    match qualifier {
        Some(q) => {
            let composite = alloc::format!("{q}.{name}");
            outer_schema
                .iter()
                .position(|sc| sc.name.eq_ignore_ascii_case(&composite))
        }
        None if bare_ok => {
            let mut found = None;
            for (i, sc) in outer_schema.iter().enumerate() {
                let bare = sc.name.rsplit('.').next().unwrap_or(sc.name.as_str());
                if bare.eq_ignore_ascii_case(name) {
                    if found.is_some() {
                        return None; // ambiguous — don't guess
                    }
                    found = Some(i);
                }
            }
            found
        }
        None => None,
    }
}

/// Materialise an outer-row value as a substitutable `Expr`. Array values
/// have no scalar `Literal` form, so they become an `ARRAY[…]`
/// constructor of element literals — this is what lets a correlated
/// `unnest(<outer array>)` expand per row.
fn outer_value_to_expr(v: Value<'static>) -> Option<Expr> {
    match v {
        Value::TextArray(items) => Some(Expr::Array(
            items
                .into_iter()
                .map(|it| {
                    Expr::Literal(match it {
                        Some(s) => spg_sql::ast::Literal::String(s),
                        None => spg_sql::ast::Literal::Null,
                    })
                })
                .collect(),
        )),
        Value::IntArray(items) => Some(Expr::Array(
            items
                .into_iter()
                .map(|it| {
                    Expr::Literal(match it {
                        Some(n) => spg_sql::ast::Literal::Integer(i64::from(n)),
                        None => spg_sql::ast::Literal::Null,
                    })
                })
                .collect(),
        )),
        Value::BigIntArray(items) => Some(Expr::Array(
            items
                .into_iter()
                .map(|it| {
                    Expr::Literal(match it {
                        Some(n) => spg_sql::ast::Literal::Integer(n),
                        None => spg_sql::ast::Literal::Null,
                    })
                })
                .collect(),
        )),
        other => value_to_literal_expr(other).ok(),
    }
}

fn substitute_outer_in_expr(
    e: &mut Expr,
    outer_row: &Row<'static>,
    outer_schema: &[ColumnSchema],
    bare_ok: bool,
) {
    if let Expr::Column(c) = e
        && let Some(idx) = outer_col_index(outer_schema, c.qualifier.as_deref(), &c.name, bare_ok)
    {
        let v = outer_row.values.get(idx).cloned().unwrap_or(Value::Null);
        if let Some(lit) = outer_value_to_expr(v) {
            *e = lit;
            return;
        }
    }
    let mut rec = |e: &mut Expr| substitute_outer_in_expr(e, outer_row, outer_schema, bare_ok);
    match e {
        Expr::Binary { lhs, rhs, .. } => {
            rec(lhs);
            rec(rhs);
        }
        Expr::Unary { expr: inner, .. } => rec(inner),
        Expr::FunctionCall { args, .. } => {
            for a in args {
                rec(a);
            }
        }
        Expr::Cast { expr: inner, .. } => rec(inner),
        // v7.38 (read01 LATERAL) — recurse the array/subscript nodes too,
        // so `LATERAL unnest(ARRAY[<outer col>, …])` and `arr[<outer>]`
        // substitute the outer reference (previously these fell through
        // to the no-op arm, stranding the reference unresolved).
        Expr::Array(items) => {
            for it in items {
                rec(it);
            }
        }
        Expr::ArraySubscript { target, index } => {
            rec(target);
            rec(index);
        }
        Expr::ArraySlice { target, lo, hi } => {
            rec(target);
            if let Some(lo) = lo {
                rec(lo);
            }
            if let Some(hi) = hi {
                rec(hi);
            }
        }
        Expr::InList { expr, list, .. } => {
            rec(expr);
            for it in list {
                rec(it);
            }
        }
        Expr::Case {
            operand,
            branches,
            else_branch,
        } => {
            if let Some(op) = operand {
                rec(op);
            }
            for (cond, val) in branches {
                rec(cond);
                rec(val);
            }
            if let Some(e) = else_branch {
                rec(e);
            }
        }
        _ => {}
    }
}

/// v7.28 (round-22) — single-table predicate pushdown + table-order
/// swap analysis, run once before the join pipeline. Splits the WHERE
/// conjuncts into per-table predicate lists (the primary plus one per
/// INNER peer) so each table can be filtered — with an index seek when
/// a conjunct is `col = literal` — BEFORE it joins. Pushed conjuncts
/// stay in WHERE too (idempotent), so correctness never depends on the
/// pushdown.
///
/// When the primary has no pushed predicate but the first INNER peer
/// does, and the swap is provably safe (equi-joins commute and output
/// columns resolve by composite name, so downstream projection is
/// order-independent; restricted to the first join with an ON whose
/// qualifiers all live in {primary, first peer}), it returns an owned
/// FromClause with the primary and that peer swapped — the join then
/// starts from the filtered side instead of cloning the whole
/// unfiltered primary (e.g. a correlated subquery body like
/// `FROM email_analysis e2 JOIN messages m2 … WHERE m2.thread_id =
/// '<outer>'`).
///
/// Returns `(swapped_from, primary_preds, peer_preds)`; `swapped_from`
/// is `Some` only when a swap happened, and the caller rebinds `from`
/// to it. The returned predicate refs borrow from `where_`.
fn analyze_join_pushdown<'w>(
    from: &FromClause,
    where_: Option<&'w Expr>,
) -> (Option<FromClause>, Vec<&'w Expr>, Vec<Vec<&'w Expr>>) {
    let primary_alias = from
        .primary
        .alias
        .as_deref()
        .unwrap_or(from.primary.name.as_str());
    let mut primary_preds: Vec<&Expr> = Vec::new();
    let mut peer_preds: Vec<Vec<&Expr>> = alloc::vec![Vec::new(); from.joins.len()];
    // v7.37.16 — a RIGHT / FULL OUTER join anywhere in the chain makes
    // the primary (left/drive) side nullable: unmatched peer rows emit
    // NULL-filled primary columns. A WHERE predicate on the primary must
    // then be applied AFTER the join, never pushed onto the primary scan
    // (pushing `l.k IS NULL` below `l RIGHT JOIN r` would filter l first
    // and wrongly keep all NULL-primary rows). Leaving such predicates in
    // the residual WHERE keeps them correct. Peer pushdown is already
    // gated to INNER peers below, so it needs no extra guard.
    let primary_nullable = from
        .joins
        .iter()
        .any(|j| matches!(j.kind, JoinKind::Right | JoinKind::FullOuter));
    if let Some(w) = where_ {
        for sub in reorder::split_and_conjunctions(w) {
            if expr_has_subquery(sub) || aggregate::contains_aggregate(sub) {
                continue;
            }
            let mut quals: Vec<&str> = Vec::new();
            let mut all_qualified = true;
            collect_column_qualifiers(sub, &mut quals, &mut all_qualified);
            if !all_qualified || quals.is_empty() {
                continue;
            }
            let q0 = quals[0];
            if !quals.iter().all(|q| q.eq_ignore_ascii_case(q0)) {
                continue;
            }
            if q0.eq_ignore_ascii_case(primary_alias) {
                if !primary_nullable {
                    primary_preds.push(sub);
                }
                continue;
            }
            for (i, j) in from.joins.iter().enumerate() {
                // v7.39 (round 588) — a comma join parses as `Cross`, and a
                // cross peer is exactly as non-nullable as an inner one, so a
                // single-relation WHERE conjunct belongs on its scan just the
                // same. Without this the `b` of `FROM a, b WHERE … b.id < 100`
                // was scanned whole.
                if matches!(j.kind, JoinKind::Inner | JoinKind::Cross)
                    && j.table.lateral_subquery.is_none()
                    && q0.eq_ignore_ascii_case(
                        j.table.alias.as_deref().unwrap_or(j.table.name.as_str()),
                    )
                {
                    peer_preds[i].push(sub);
                    break;
                }
            }
        }
    }
    // Safety: swapping reorders which table joins FIRST, so it is only
    // legal when the FIRST join's ON references no table beyond
    // {primary, first peer} (a later peer's ON may name the original
    // primary, which must already be in the combined row when that peer
    // joins). Restrict to i == 0 AND an ON whose qualifiers all live in
    // those two tables.
    if primary_preds.is_empty()
        && let Some(j0) = from.joins.first()
        && matches!(j0.kind, JoinKind::Inner)
        && j0.table.lateral_subquery.is_none()
        && !peer_preds[0].is_empty()
    {
        let peer_alias = j0.table.alias.as_deref().unwrap_or(j0.table.name.as_str());
        let on_safe = j0.on.as_ref().is_some_and(|on| {
            let mut quals: Vec<&str> = Vec::new();
            let mut all_q = true;
            collect_column_qualifiers(on, &mut quals, &mut all_q);
            all_q
                && quals.iter().all(|q| {
                    q.eq_ignore_ascii_case(primary_alias) || q.eq_ignore_ascii_case(peer_alias)
                })
        });
        if on_safe {
            let mut from_owned = from.clone();
            core::mem::swap(&mut from_owned.primary, &mut from_owned.joins[0].table);
            let primary_preds = peer_preds[0].drain(..).collect();
            return (Some(from_owned), primary_preds, peer_preds);
        }
    }
    (None, primary_preds, peer_preds)
}

/// Build the combined output schema for a join: every primary column
/// then every peer column, each qualified `<alias>.<col>` so the
/// deferred-join cell lookups and downstream projection resolve by
/// composite name.
fn build_combined_schema(
    primary_alias: &str,
    primary_cols: &[ColumnSchema],
    joined: &[JoinedPeer<'_>],
) -> Vec<ColumnSchema> {
    let mut combined_schema: Vec<ColumnSchema> = Vec::new();
    for col in primary_cols {
        combined_schema.push(ColumnSchema::new(
            alloc::format!("{primary_alias}.{}", col.name),
            col.ty,
            col.nullable,
        ));
    }
    for peer in joined {
        for col in &peer.cols {
            combined_schema.push(ColumnSchema::new(
                alloc::format!("{}.{}", peer.alias, col.name),
                col.ty,
                col.nullable,
            ));
        }
    }
    combined_schema
}

/// v7.37.x — helper for `try_count_star_left_anti_join_fast`. Recognises
/// `outer.X = inner.Y` (commuted accepted) and returns the column names.
fn analyse_join_eq(
    on: &Expr,
    outer_alias: &str,
    inner_alias: &str,
) -> Result<Option<(String, String)>, EngineError> {
    use spg_sql::ast::BinOp;
    let Expr::Binary {
        lhs,
        op: BinOp::Eq,
        rhs,
    } = on
    else {
        return Ok(None);
    };
    let (Expr::Column(a), Expr::Column(b)) = (lhs.as_ref(), rhs.as_ref()) else {
        return Ok(None);
    };
    fn col_alias(c: &spg_sql::ast::ColumnName) -> Option<&str> {
        c.qualifier.as_deref()
    }
    let pair_o_then_i = (col_alias(a), col_alias(b));
    if matches!(pair_o_then_i, (Some(aq), Some(bq))
        if aq.eq_ignore_ascii_case(outer_alias) && bq.eq_ignore_ascii_case(inner_alias))
    {
        return Ok(Some((a.name.clone(), b.name.clone())));
    }
    if matches!(pair_o_then_i, (Some(aq), Some(bq))
        if aq.eq_ignore_ascii_case(inner_alias) && bq.eq_ignore_ascii_case(outer_alias))
    {
        return Ok(Some((b.name.clone(), a.name.clone())));
    }
    Ok(None)
}

/// v7.37.x — recognise `<inner_alias>.<inner_col> IS NULL`.
fn is_inner_is_null(e: &Expr, inner_alias: &str, inner_col: &str) -> bool {
    let Expr::IsNull { expr, negated } = e else {
        return false;
    };
    if *negated {
        return false;
    }
    let Expr::Column(c) = expr.as_ref() else {
        return false;
    };
    c.qualifier
        .as_deref()
        .is_some_and(|q| q.eq_ignore_ascii_case(inner_alias))
        && c.name.eq_ignore_ascii_case(inner_col)
}

pub static ANTI_JOIN_FAST_PATH_TRIED: core::sync::atomic::AtomicU64 =
    core::sync::atomic::AtomicU64::new(0);
pub static ANTI_JOIN_FAST_PATH_FIRED: core::sync::atomic::AtomicU64 =
    core::sync::atomic::AtomicU64::new(0);
