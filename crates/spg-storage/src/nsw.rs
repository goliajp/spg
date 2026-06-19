//! HNSW vector-index algorithms: insert/connect, greedy + beam
//! search, neighbour heuristics, and the f32/SQ8 distance
//! kernels. Split out of lib.rs (monster tier-3 cut 2). The
//! `NswGraph` data type itself stays in lib.rs as index
//! vocabulary; this module is the algorithms over it.

use super::*;

/// Insert one row into the HNSW graph held by index slot `idx_pos`.
/// No-op when the row's value at the indexed column isn't a vector.
/// v6.0.1: handles `Value::Sq8Vector` by dequantising into an f32
/// "query" surface — the existing greedy + beam-search machinery
/// then uses `cell_to_query_metric_distance` to route every
/// distance call through the cell's actual encoding.
pub(crate) fn nsw_insert_at(table: &mut Table, idx_pos: usize, new_row_idx: usize) {
    let col_pos = table.indices[idx_pos].column_position;
    let cell_dim: Option<usize> = match &table.rows[new_row_idx].values[col_pos] {
        Value::Vector(v) => Some(v.len()),
        Value::Sq8Vector(q) => Some(q.bytes.len()),
        Value::HalfVector(h) => Some(h.dim()),
        _ => None,
    };
    let Some(dim) = cell_dim else {
        // Even non-vector rows occupy a level slot so per-node Vec
        // lengths stay aligned with `table.rows.len()`.
        ensure_node_slot(table, idx_pos, new_row_idx, 0);
        return;
    };
    if dim == 0 {
        ensure_node_slot(table, idx_pos, new_row_idx, 0);
        return;
    }
    let level = nsw_assign_level(new_row_idx);
    ensure_node_slot(table, idx_pos, new_row_idx, level);
    let (entry, entry_level, m) = match &table.indices[idx_pos].kind {
        IndexKind::Nsw(g) => (g.entry, g.entry_level, g.m),
        IndexKind::BTree(_)
        | IndexKind::Brin { .. }
        | IndexKind::Gin(_)
        | IndexKind::GinTrgm(_)
        | IndexKind::GinFulltext(_)
        | IndexKind::GinJsonb(_) => {
            unreachable!("nsw_insert_at on a non-NSW index")
        }
    };
    // First node ever — declare it the entry (it gets its own level).
    if entry.is_none() {
        if let IndexKind::Nsw(g) = &mut table.indices[idx_pos].kind {
            g.entry = Some(new_row_idx);
            g.entry_level = level;
            *g.levels
                .get_mut(new_row_idx)
                .expect("levels slot padded by ensure_node_slot") = level;
        }
        return;
    }
    // Set the node's recorded level.
    if let IndexKind::Nsw(g) = &mut table.indices[idx_pos].kind {
        *g.levels
            .get_mut(new_row_idx)
            .expect("levels slot padded by ensure_node_slot") = level;
    }
    let query = match &table.rows[new_row_idx].values[col_pos] {
        Value::Vector(v) => v.clone(),
        // v6.0.1: dequantise the inserted SQ8 cell into an f32 query
        // surface so the existing greedy / beam machinery can route
        // distances through `cell_to_query_metric_distance`. The
        // small dequantisation error is what the recall@10 ≥ 0.95
        // envelope already accounts for (V6_DESIGN deliberation #3).
        Value::Sq8Vector(q) => quantize::dequantize(q),
        // v6.0.3: halfvec dequant is bit-exact at the storage layer,
        // so the inserted query is a faithful representation.
        Value::HalfVector(h) => h.to_f32_vec(),
        _ => return,
    };
    // Phase 1: greedy descend from `entry` down to `level + 1`, keeping
    // exactly one current best so the next layer starts from it.
    let mut current = entry.expect("entry was Some above");
    let mut current_d = vec_l2_sq(table, col_pos, current, &query);
    if entry_level > level {
        for layer in (level + 1..=entry_level).rev() {
            (current, current_d) =
                greedy_layer_walk(table, idx_pos, layer, current, current_d, &query);
        }
    }
    // Phase 2: from `min(level, entry_level)` down to 0, beam-search
    // `ef_construction` candidates, run the HNSW §4 heuristic neighbour
    // selection over them, and connect bidirectionally.
    let top = level.min(entry_level);
    let ef = (m * 2).max(8);
    for layer in (0..=top).rev() {
        let cap = if layer == 0 { m * 2 } else { m };
        let mut candidates = layer_beam_search(
            table,
            idx_pos,
            layer,
            current,
            current_d,
            &query,
            ef,
            NswMetric::L2,
        );
        candidates.retain(|&(_, n)| n != new_row_idx);
        // Take the closest as the entry for the next layer down — done
        // before heuristic narrowing because the heuristic can reorder.
        if let Some(&(d, n)) = candidates.first() {
            current = n;
            current_d = d;
        }
        let peers = select_neighbours_heuristic(&candidates, cap, table, col_pos);
        connect_at_layer(table, idx_pos, layer, new_row_idx, &peers);
    }
    // Phase 3: if the new node climbed above the current entry, take
    // over as entry so future inserts/searches start from the new top.
    if level > entry_level
        && let IndexKind::Nsw(g) = &mut table.indices[idx_pos].kind
    {
        g.entry = Some(new_row_idx);
        g.entry_level = level;
    }
}

/// Make sure `layers[*][new_row_idx]` and `levels[new_row_idx]` exist,
/// padding with empty/zero entries as needed. Also grows `layers` to
/// accommodate the node's top `level`.
fn ensure_node_slot(table: &mut Table, idx_pos: usize, new_row_idx: usize, level: u8) {
    let IndexKind::Nsw(g) = &mut table.indices[idx_pos].kind else {
        unreachable!("ensure_node_slot on a BTree index");
    };
    while g.layers.len() <= level as usize {
        g.layers.push(PersistentVec::new());
    }
    while g.levels.len() <= new_row_idx {
        g.levels.push_mut(0);
    }
    for layer_vec in &mut g.layers {
        while layer_vec.len() <= new_row_idx {
            layer_vec.push_mut(Vec::new());
        }
    }
}

/// Single-step greedy walk on one layer: from `current` (with cached
/// distance `current_d`), inspect that node's neighbours at `layer` and
/// hop to the closest if it beats `current_d`. Repeat until no move
/// improves the distance. Cheap variant of beam-search used for the
/// "descend" phase that only needs one survivor per layer.
fn greedy_layer_walk(
    table: &Table,
    idx_pos: usize,
    layer: u8,
    mut current: usize,
    mut current_d: f32,
    query: &[f32],
) -> (usize, f32) {
    let g = match &table.indices[idx_pos].kind {
        IndexKind::Nsw(g) => g,
        IndexKind::BTree(_)
        | IndexKind::Brin { .. }
        | IndexKind::Gin(_)
        | IndexKind::GinTrgm(_)
        | IndexKind::GinFulltext(_)
        | IndexKind::GinJsonb(_) => {
            return (current, current_d);
        }
    };
    let col_pos = table.indices[idx_pos].column_position;
    loop {
        let neighbours: &[u32] = g
            .layers
            .get(layer as usize)
            .and_then(|layer_v| layer_v.get(current))
            .map_or(&[][..], Vec::as_slice);
        let mut best = current;
        let mut best_d = current_d;
        for &n in neighbours {
            let n = n as usize;
            let d = vec_l2_sq(table, col_pos, n, query);
            if d < best_d {
                best = n;
                best_d = d;
            }
        }
        if best == current {
            return (current, current_d);
        }
        current = best;
        current_d = best_d;
    }
}

/// Beam search on one layer starting from `entry_node` with cached
/// `entry_d`. Returns the top `ef` candidates in ascending-distance
/// order. Caller picks the closest as the next layer's entry and / or
/// trims to M for connection.
///
/// v3.0.1: uses two `BinaryHeap`s (min-heap for the open frontier,
/// max-heap for the working top-`ef` results) and a `Vec<bool>` visited
/// bitmap, replacing the v2.x `Vec` + `partition_point` + `BTreeSet`
/// implementation. Same algorithm shape (HNSW search algorithm 2 from
/// the paper); the data-structure swap cuts per-visit cost from
/// `O(ef + log row_count)` to amortised `O(log ef)`.
#[allow(clippy::too_many_arguments)] // Beam search threads layer, entry, query, ef, metric — each is intrinsic. Bundling them into a config struct hides the call sites.
fn layer_beam_search(
    table: &Table,
    idx_pos: usize,
    layer: u8,
    entry_node: usize,
    entry_d: f32,
    query: &[f32],
    ef: usize,
    metric: NswMetric,
) -> Vec<(f32, usize)> {
    let g = match &table.indices[idx_pos].kind {
        IndexKind::Nsw(g) => g,
        IndexKind::BTree(_)
        | IndexKind::Brin { .. }
        | IndexKind::Gin(_)
        | IndexKind::GinTrgm(_)
        | IndexKind::GinFulltext(_)
        | IndexKind::GinJsonb(_) => return Vec::new(),
    };
    let col_pos = table.indices[idx_pos].column_position;
    let d0 = if matches!(metric, NswMetric::L2) {
        entry_d
    } else {
        cell_to_query_metric_distance(table, col_pos, entry_node, query, metric)
    };
    let row_count = table.rows.len();
    let mut visited: Vec<bool> = alloc::vec![false; row_count];
    if entry_node < row_count {
        visited[entry_node] = true;
    }
    // candidates: min-heap by distance (Closest wrapper) — frontier
    // results:    max-heap by distance (Furthest wrapper) — top-ef working set
    let mut candidates: alloc::collections::BinaryHeap<NodeClosest> =
        alloc::collections::BinaryHeap::with_capacity(ef);
    let mut results: alloc::collections::BinaryHeap<NodeFurthest> =
        alloc::collections::BinaryHeap::with_capacity(ef);
    candidates.push(NodeClosest {
        dist: d0,
        node: entry_node,
    });
    results.push(NodeFurthest {
        dist: d0,
        node: entry_node,
    });
    while let Some(cur) = candidates.pop() {
        let worst = results.peek().map_or(f32::INFINITY, |c| c.dist);
        if cur.dist > worst && results.len() >= ef {
            break;
        }
        let neighbours: &[u32] = g
            .layers
            .get(layer as usize)
            .and_then(|layer_v| layer_v.get(cur.node))
            .map_or(&[][..], Vec::as_slice);
        for &n in neighbours {
            let n = n as usize;
            if n >= row_count || visited[n] {
                continue;
            }
            visited[n] = true;
            // v6.0.1: cell-aware distance — F32 cells take the
            // existing scalar metric, SQ8 cells route through
            // the asymmetric ADC variant for the same metric.
            let dn = cell_to_query_metric_distance(table, col_pos, n, query, metric);
            if !dn.is_finite() {
                continue;
            }
            let worst = results.peek().map_or(f32::INFINITY, |c| c.dist);
            if results.len() < ef || dn < worst {
                results.push(NodeFurthest { dist: dn, node: n });
                if results.len() > ef {
                    results.pop();
                }
                candidates.push(NodeClosest { dist: dn, node: n });
            }
        }
    }
    // Drain results (max-heap order) and re-sort ascending so callers
    // can take `closest = result[0]` without flipping.
    let mut out: Vec<(f32, usize)> = results.into_iter().map(|c| (c.dist, c.node)).collect();
    out.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(core::cmp::Ordering::Equal));
    out
}

/// Min-heap wrapper: smaller `dist` → higher priority in a `BinaryHeap`
/// (which is a max-heap), so we flip the comparison. NaN sorts last
/// (lowest priority) to keep the heap total-ordered.
#[derive(Debug, Clone, Copy)]
struct NodeClosest {
    dist: f32,
    node: usize,
}
impl PartialEq for NodeClosest {
    fn eq(&self, other: &Self) -> bool {
        self.dist == other.dist && self.node == other.node
    }
}
impl Eq for NodeClosest {}
impl PartialOrd for NodeClosest {
    fn partial_cmp(&self, other: &Self) -> Option<core::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for NodeClosest {
    fn cmp(&self, other: &Self) -> core::cmp::Ordering {
        // Reversed: smaller dist = greater priority.
        other
            .dist
            .partial_cmp(&self.dist)
            .unwrap_or(core::cmp::Ordering::Equal)
    }
}

/// Max-heap wrapper: larger `dist` sits at the top so the worst result
/// can be evicted in O(log n) when a better candidate arrives.
#[derive(Debug, Clone, Copy)]
struct NodeFurthest {
    dist: f32,
    node: usize,
}
impl PartialEq for NodeFurthest {
    fn eq(&self, other: &Self) -> bool {
        self.dist == other.dist && self.node == other.node
    }
}
impl Eq for NodeFurthest {}
impl PartialOrd for NodeFurthest {
    fn partial_cmp(&self, other: &Self) -> Option<core::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for NodeFurthest {
    fn cmp(&self, other: &Self) -> core::cmp::Ordering {
        self.dist
            .partial_cmp(&other.dist)
            .unwrap_or(core::cmp::Ordering::Equal)
    }
}

/// HNSW paper §4 algorithm 4: pick `m` neighbours from `candidates` so
/// that each chosen point isn't already covered by a closer chosen
/// point. Improves graph diversity → fewer hops needed at search time.
///
/// `candidates` arrives sorted ascending by distance-to-query. We walk
/// it in order, keeping a candidate only when no already-chosen point
/// is closer to it than the query is. Result is a vector of row
/// indices (length ≤ `m`).
fn select_neighbours_heuristic(
    candidates: &[(f32, usize)],
    m: usize,
    table: &Table,
    col_pos: usize,
) -> Vec<usize> {
    let mut chosen: Vec<usize> = Vec::with_capacity(m);
    for &(d_q, e) in candidates {
        if chosen.len() >= m {
            break;
        }
        // v6.0.1: works on either `Value::Vector` (F32) or
        // `Value::Sq8Vector` (Sq8) cells — `cell_l2_sq` dispatches
        // on encoding. A non-vector cell yields `f32::INFINITY`
        // which the `< d_q` test will never accept.
        if !matches!(
            table.rows.get(e).and_then(|r| r.values.get(col_pos)),
            Some(Value::Vector(_) | Value::Sq8Vector(_) | Value::HalfVector(_))
        ) {
            continue;
        }
        let mut covered = false;
        for &r in &chosen {
            // dist(e, r) measured in the same metric the topology was
            // built with (L2). If a chosen `r` is closer to `e` than
            // the query is, `r` already "covers" `e` for navigation.
            if cell_l2_sq(table, col_pos, e, r) < d_q {
                covered = true;
                break;
            }
        }
        if !covered {
            chosen.push(e);
        }
    }
    chosen
}

/// Bidirectionally connect `new_row_idx` to each of `peers` at `layer`,
/// trimming each endpoint's adjacency to that layer's degree cap by
/// keeping only the closest neighbours.
fn connect_at_layer(
    table: &mut Table,
    idx_pos: usize,
    layer: u8,
    new_row_idx: usize,
    peers: &[usize],
) {
    let col_pos = table.indices[idx_pos].column_position;
    let cap = match &table.indices[idx_pos].kind {
        IndexKind::Nsw(g) => g.cap_for_layer(layer),
        IndexKind::BTree(_)
        | IndexKind::Brin { .. }
        | IndexKind::Gin(_)
        | IndexKind::GinTrgm(_)
        | IndexKind::GinFulltext(_)
        | IndexKind::GinJsonb(_) => return,
    };
    // v6.1.x: NSW adjacency stores neighbour row indices as u32 (4 B
    // each) rather than usize (8 B on 64-bit). Boundary casts here
    // assert the row count fits in u32 — the catalog already enforces
    // ≤ 4G rows per table, so the conversion can't lose data.
    let new_row_u32 = u32::try_from(new_row_idx).expect("row index fits in u32");
    if let IndexKind::Nsw(g) = &mut table.indices[idx_pos].kind {
        let layer_v = &mut g.layers[layer as usize];
        if let Some(slot) = layer_v.get_mut(new_row_idx) {
            *slot = peers
                .iter()
                .map(|&p| u32::try_from(p).expect("row index fits in u32"))
                .collect();
        }
    }
    for &peer in peers {
        // Skip peers whose indexed cell isn't a vector — same fence
        // as the F32 path; SQ8 cells flow through `cell_l2_sq`
        // below without dequantising.
        if !matches!(
            &table.rows[peer].values[col_pos],
            Value::Vector(_) | Value::Sq8Vector(_) | Value::HalfVector(_)
        ) {
            continue;
        }
        // 1. add the new node to peer's adjacency
        if let IndexKind::Nsw(g) = &mut table.indices[idx_pos].kind {
            let layer_v = &mut g.layers[layer as usize];
            if let Some(slot) = layer_v.get_mut(peer)
                && !slot.contains(&new_row_u32)
            {
                slot.push(new_row_u32);
            }
        }
        // 2. if peer is over budget, rebuild its adjacency with the
        //    HNSW §4 heuristic — same diversity criterion as the
        //    insert path so connectivity stays consistent.
        let needs_trim = match &table.indices[idx_pos].kind {
            IndexKind::Nsw(g) => g.layers[layer as usize][peer].len() > cap,
            IndexKind::BTree(_)
            | IndexKind::Brin { .. }
            | IndexKind::Gin(_)
            | IndexKind::GinTrgm(_)
            | IndexKind::GinFulltext(_)
            | IndexKind::GinJsonb(_) => false,
        };
        if needs_trim {
            let current_peers: Vec<usize> = match &table.indices[idx_pos].kind {
                IndexKind::Nsw(g) => g.layers[layer as usize][peer]
                    .iter()
                    .map(|&n| n as usize)
                    .collect(),
                IndexKind::BTree(_)
                | IndexKind::Brin { .. }
                | IndexKind::Gin(_)
                | IndexKind::GinTrgm(_)
                | IndexKind::GinFulltext(_)
                | IndexKind::GinJsonb(_) => continue,
            };
            // Sort by distance from `peer`'s cell ascending so the
            // heuristic receives candidates closest-first. `cell_l2_sq`
            // dispatches on encoding so SQ8 columns trim using
            // symmetric ADC.
            let mut tagged: Vec<(f32, usize)> = current_peers
                .iter()
                .map(|&p| (cell_l2_sq(table, col_pos, peer, p), p))
                .collect();
            tagged.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(core::cmp::Ordering::Equal));
            let kept = select_neighbours_heuristic(&tagged, cap, table, col_pos);
            if let IndexKind::Nsw(g) = &mut table.indices[idx_pos].kind
                && let Some(slot) = g.layers[layer as usize].get_mut(peer)
            {
                *slot = kept
                    .into_iter()
                    .map(|p| u32::try_from(p).expect("row index fits in u32"))
                    .collect();
            }
        }
    }
}

/// Squared L2 distance from `query` (raw f32) to the cell at
/// `(row, col_pos)`. Dispatches on cell encoding: `Value::Vector`
/// (F32) uses `l2_distance_sq`; `Value::Sq8Vector` uses
/// `sq8_l2_distance_sq_asymmetric` (the v6.0.1 quantised path).
/// Returns `f32::INFINITY` for any non-vector cell so callers can
/// compare uniformly.
fn vec_l2_sq(table: &Table, col_pos: usize, row: usize, query: &[f32]) -> f32 {
    match table.rows.get(row).and_then(|r| r.values.get(col_pos)) {
        Some(Value::Vector(v)) if v.len() == query.len() => l2_distance_sq(v, query),
        Some(Value::Sq8Vector(q)) if q.bytes.len() == query.len() => {
            quantize::sq8_l2_distance_sq_asymmetric(q, query)
        }
        // v6.0.6: halfvec → fused NEON SIMD kernel; no Vec<f32>
        // allocation. v6.0.3 used `to_f32_vec()` + f32 NEON which
        // was correct but allocated per call (5× slower than F32).
        Some(Value::HalfVector(h)) if h.dim() == query.len() => {
            halfvec::half_l2_distance_sq_asymmetric(h, query)
        }
        _ => f32::INFINITY,
    }
}

/// Squared L2 distance between two stored cells (no f32 query in
/// sight). Used during HNSW graph build — both endpoints are
/// rows already in the table, so symmetric ADC applies for SQ8
/// columns. Mixed-encoding cells within one column are a
/// schema-level impossibility (INSERT-time coercion enforces
/// uniform encoding), so the catch-all is an abort.
fn cell_l2_sq(table: &Table, col_pos: usize, row_a: usize, row_b: usize) -> f32 {
    let Some(cell_a) = table.rows.get(row_a).and_then(|r| r.values.get(col_pos)) else {
        return f32::INFINITY;
    };
    let Some(cell_b) = table.rows.get(row_b).and_then(|r| r.values.get(col_pos)) else {
        return f32::INFINITY;
    };
    match (cell_a, cell_b) {
        (Value::Vector(a), Value::Vector(b)) if a.len() == b.len() => l2_distance_sq(a, b),
        (Value::Sq8Vector(a), Value::Sq8Vector(b)) if a.bytes.len() == b.bytes.len() => {
            quantize::sq8_l2_distance_sq(a, b)
        }
        // v6.0.6: halfvec symmetric NEON — fused SIMD kernel that
        // loads both cells' raw u16 bits, expands to f32 lanes
        // inline, FMA-accumulates the squared diff. No Vec<f32>
        // allocation per call.
        (Value::HalfVector(a), Value::HalfVector(b)) if a.dim() == b.dim() => {
            halfvec::half_l2_distance_sq(a, b)
        }
        _ => f32::INFINITY,
    }
}

/// kNN-search-time distance: stored cell → f32 query under the
/// caller's metric. Dispatches on cell encoding so SQ8 columns
/// take the ADC path with the right asymmetric variant. NaN /
/// dim-mismatch / non-vector → `f32::INFINITY`.
fn cell_to_query_metric_distance(
    table: &Table,
    col_pos: usize,
    row: usize,
    query: &[f32],
    metric: NswMetric,
) -> f32 {
    match table.rows.get(row).and_then(|r| r.values.get(col_pos)) {
        Some(Value::Vector(v)) if v.len() == query.len() => metric_distance(metric, v, query),
        Some(Value::Sq8Vector(q)) if q.bytes.len() == query.len() => match metric {
            NswMetric::L2 => quantize::sq8_l2_distance_sq_asymmetric(q, query),
            NswMetric::InnerProduct => quantize::sq8_inner_product_asymmetric(q, query),
            NswMetric::Cosine => quantize::sq8_cosine_distance_asymmetric(q, query),
        },
        // v6.0.6: halfvec dispatches by metric to fused NEON
        // kernels — no Vec<f32> allocation per call.
        Some(Value::HalfVector(h)) if h.dim() == query.len() => match metric {
            NswMetric::L2 => halfvec::half_l2_distance_sq_asymmetric(h, query),
            NswMetric::InnerProduct => halfvec::half_inner_product_asymmetric(h, query),
            NswMetric::Cosine => halfvec::half_cosine_distance_asymmetric(h, query),
        },
        _ => f32::INFINITY,
    }
}

/// Distance metric used at NSW search time. The graph topology is
/// always built with `L2`; querying with `InnerProduct` / `Cosine`
/// reuses the same edges but ranks candidates by the chosen metric.
/// For the corpus-sized graphs this loses negligible recall vs
/// building separate per-metric graphs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NswMetric {
    /// Squared Euclidean — ranks "smaller = closer" (the sqrt is
    /// monotonic so we skip it for ordering).
    L2,
    /// Negated dot product, matching pgvector `<#>` convention so
    /// "smaller = more similar" holds across all three metrics.
    InnerProduct,
    /// Cosine distance `1 - cos(a, b)`. Zero-norm operand yields
    /// `f32::INFINITY` so it sorts last.
    Cosine,
}

/// Multi-layer HNSW kNN search: greedy-descend from the entry to layer 0,
/// then beam-search there with the requested `ef` to return the top `k`
/// results under the caller-chosen metric. Topology was built with L2 —
/// upper-layer descent uses L2 as a coarse heuristic; final beam search
/// runs in the requested metric so rankings are correct for `<#>` / `<=>`.
pub(crate) fn nsw_search(
    table: &Table,
    idx_pos: usize,
    query: &[f32],
    k: usize,
    ef: usize,
    metric: NswMetric,
) -> Vec<(f32, usize)> {
    let (entry, entry_level) = match &table.indices[idx_pos].kind {
        IndexKind::Nsw(g) => (g.entry, g.entry_level),
        IndexKind::BTree(_)
        | IndexKind::Brin { .. }
        | IndexKind::Gin(_)
        | IndexKind::GinTrgm(_)
        | IndexKind::GinFulltext(_)
        | IndexKind::GinJsonb(_) => return Vec::new(),
    };
    let Some(entry) = entry else {
        return Vec::new();
    };
    let col_pos = table.indices[idx_pos].column_position;
    // v6.0.1 step 5: SQ8 columns over-fetch by `SQ8_RERANK_OVER_FETCH`
    // so the rerank pass below sees enough candidates to recover
    // recall after the ADC re-ordering. F32 + F16 columns skip the
    // over-fetch — F32 distances are exact, F16 dequant is
    // bit-exact at the storage layer so the beam search already
    // ranks under the column's full precision.
    let sq8 = matches!(
        table.schema.columns.get(col_pos).map(|c| c.ty),
        Some(DataType::Vector {
            encoding: VecEncoding::Sq8,
            ..
        })
    );
    let ef = if sq8 {
        ef.max(k).max(k * SQ8_RERANK_OVER_FETCH)
    } else {
        ef.max(k)
    };
    // Descend by L2 (the topology metric) so layers prune consistently.
    let entry_d = vec_l2_sq(table, col_pos, entry, query);
    let mut current = entry;
    let mut current_d = entry_d;
    for layer in (1..=entry_level).rev() {
        (current, current_d) = greedy_layer_walk(table, idx_pos, layer, current, current_d, query);
    }
    // Final beam search on layer 0 under the caller's metric.
    let mut results = layer_beam_search(table, idx_pos, 0, current, current_d, query, ef, metric);
    if sq8 {
        results = sq8_rerank(table, col_pos, &results, query, metric);
    }
    results.truncate(k);
    results
}

/// v6.0.1 step 5: re-score ADC top-`K*3` candidates with the
/// dequantised cell vs the f32 query, then re-sort. Recovers the
/// recall the SQ8 ADC sacrifices for 4× compression — the design's
/// "f32 rerank step is on by default" path (deliberation #3).
/// `metric` is the same metric the beam search used; the rerank
/// arithmetic re-derives the exact distance under that metric.
fn sq8_rerank(
    table: &Table,
    col_pos: usize,
    candidates: &[(f32, usize)],
    query: &[f32],
    metric: NswMetric,
) -> Vec<(f32, usize)> {
    let mut out: Vec<(f32, usize)> = candidates
        .iter()
        .filter_map(|&(adc_d, row)| {
            let cell = table.rows.get(row).and_then(|r| r.values.get(col_pos))?;
            let Value::Sq8Vector(q) = cell else {
                // F32 cells shouldn't reach this path (sq8 fence
                // above), but stay defensive: pass through with
                // the ADC distance unchanged.
                return Some((adc_d, row));
            };
            let deq = quantize::dequantize(q);
            if deq.len() != query.len() {
                return None;
            }
            Some((metric_distance(metric, &deq, query), row))
        })
        .collect();
    out.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(core::cmp::Ordering::Equal));
    out
}

/// Multiplier applied to `k` so the SQ8 rerank pass sees a wider
/// candidate set. 3× is the design-stage value; v6.0.5 sweep work
/// can re-tune once full corpus profiling is in.
const SQ8_RERANK_OVER_FETCH: usize = 3;

fn metric_distance(metric: NswMetric, a: &[f32], b: &[f32]) -> f32 {
    match metric {
        NswMetric::L2 => l2_distance_sq(a, b),
        NswMetric::InnerProduct => -inner_product_f32(a, b),
        NswMetric::Cosine => {
            let (dot, na, nb) = cosine_dot_norms_f32(a, b);
            if na == 0.0 || nb == 0.0 {
                return f32::INFINITY;
            }
            // `f32::sqrt` lives in std, so hand-roll Newton-Raphson on
            // f64 — same trick the L2 binary op already uses.
            let denom = sqrt_newton_f32(na) * sqrt_newton_f32(nb);
            1.0 - dot / denom
        }
    }
}

/// v6.0.2: dispatch wrapper for the f32 dot product (used by `<#>` +
/// the cosine numerator). NEON path when `len % 4 == 0 && len >= 4`,
/// scalar fallback otherwise. Returns the positive dot — callers
/// negate for the pgvector `<#>` "smaller = closer" convention.
///
/// Public so perf gates + downstream benches can microbenchmark the
/// dispatch directly; not part of the STABILITY contract — internal
/// SIMD layout can evolve in any release.
#[doc(hidden)]
#[inline]
pub fn inner_product_f32(a: &[f32], b: &[f32]) -> f32 {
    #[cfg(target_arch = "aarch64")]
    {
        if a.len() == b.len() && a.len() >= 4 && a.len().is_multiple_of(4) {
            // SAFETY: NEON is a baseline aarch64 feature; preconditions
            // (matching lengths, ≥ 1 full lane group) are checked above.
            return unsafe { inner_product_neon(a, b) };
        }
    }
    inner_product_scalar(a, b)
}

pub(crate) fn inner_product_scalar(a: &[f32], b: &[f32]) -> f32 {
    let mut dot: f32 = 0.0;
    for (x, y) in a.iter().zip(b.iter()) {
        dot += x * y;
    }
    dot
}

#[cfg(target_arch = "aarch64")]
#[target_feature(enable = "neon")]
#[allow(clippy::many_single_char_names)] // NEON intrinsics work in single-letter regs by convention
pub(crate) unsafe fn inner_product_neon(a: &[f32], b: &[f32]) -> f32 {
    use core::arch::aarch64::{
        float32x4_t, vaddq_f32, vaddvq_f32, vdupq_n_f32, vfmaq_f32, vld1q_f32,
    };
    unsafe {
        // Two parallel accumulators (same trick as L2 NEON) so the
        // FMA dependency chain doesn't serialise.
        let zero: float32x4_t = vdupq_n_f32(0.0);
        let mut acc0 = zero;
        let mut acc1 = zero;
        let n = a.len();
        let mut i = 0usize;
        while i + 8 <= n {
            let av0 = vld1q_f32(a.as_ptr().add(i));
            let bv0 = vld1q_f32(b.as_ptr().add(i));
            acc0 = vfmaq_f32(acc0, av0, bv0);
            let av1 = vld1q_f32(a.as_ptr().add(i + 4));
            let bv1 = vld1q_f32(b.as_ptr().add(i + 4));
            acc1 = vfmaq_f32(acc1, av1, bv1);
            i += 8;
        }
        while i + 4 <= n {
            let av = vld1q_f32(a.as_ptr().add(i));
            let bv = vld1q_f32(b.as_ptr().add(i));
            acc0 = vfmaq_f32(acc0, av, bv);
            i += 4;
        }
        vaddvq_f32(vaddq_f32(acc0, acc1))
    }
}

/// v6.0.2: dispatch wrapper for the three accumulators (`dot`, `||a||²`,
/// `||b||²`) cosine needs. Same NEON pre-condition as the L2 / IP
/// paths; same scalar fallback shape.
///
/// Public for benchmarking only (see `inner_product_f32`); not in the
/// STABILITY contract.
#[doc(hidden)]
#[inline]
pub fn cosine_dot_norms_f32(a: &[f32], b: &[f32]) -> (f32, f32, f32) {
    #[cfg(target_arch = "aarch64")]
    {
        if a.len() == b.len() && a.len() >= 4 && a.len().is_multiple_of(4) {
            // SAFETY: see `inner_product_neon`.
            return unsafe { cosine_dot_norms_neon(a, b) };
        }
    }
    cosine_dot_norms_scalar(a, b)
}

pub(crate) fn cosine_dot_norms_scalar(a: &[f32], b: &[f32]) -> (f32, f32, f32) {
    let mut dot: f32 = 0.0;
    let mut na: f32 = 0.0;
    let mut nb: f32 = 0.0;
    for (x, y) in a.iter().zip(b.iter()) {
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    (dot, na, nb)
}

#[cfg(target_arch = "aarch64")]
#[target_feature(enable = "neon")]
#[allow(clippy::many_single_char_names, clippy::similar_names)]
pub(crate) unsafe fn cosine_dot_norms_neon(a: &[f32], b: &[f32]) -> (f32, f32, f32) {
    use core::arch::aarch64::{float32x4_t, vaddvq_f32, vdupq_n_f32, vfmaq_f32, vld1q_f32};
    unsafe {
        let zero: float32x4_t = vdupq_n_f32(0.0);
        let mut acc_dot = zero;
        let mut acc_na = zero;
        let mut acc_nb = zero;
        let n = a.len();
        let mut i = 0usize;
        while i + 4 <= n {
            let av = vld1q_f32(a.as_ptr().add(i));
            let bv = vld1q_f32(b.as_ptr().add(i));
            acc_dot = vfmaq_f32(acc_dot, av, bv);
            acc_na = vfmaq_f32(acc_na, av, av);
            acc_nb = vfmaq_f32(acc_nb, bv, bv);
            i += 4;
        }
        (vaddvq_f32(acc_dot), vaddvq_f32(acc_na), vaddvq_f32(acc_nb))
    }
}

fn sqrt_newton_f32(x: f32) -> f32 {
    if x <= 0.0 {
        return 0.0;
    }
    let mut g = x;
    for _ in 0..10 {
        g = 0.5 * (g + x / g);
    }
    g
}

/// Squared Euclidean distance — used for ordering inside NSW (the sqrt
/// preserves the order). Caller takes sqrt before reporting back to SQL.
///
/// v3.3.2: aarch64 NEON path for `len % 4 == 0` (which covers every
/// HNSW-indexed VECTOR(N) where N is a multiple of 4 — i.e. all
/// production-shaped embeddings: 64, 128, 256, 384, 512, 768, 1024,
/// 1536, ...). Other shapes fall back to the scalar loop.
#[inline]
pub(crate) fn l2_distance_sq(a: &[f32], b: &[f32]) -> f32 {
    #[cfg(target_arch = "aarch64")]
    {
        if a.len() == b.len() && a.len() >= 4 && a.len().is_multiple_of(4) {
            // SAFETY: NEON is a baseline aarch64 feature (ARMv8);
            // the precondition is checked above (matching lengths,
            // multiple of 4, at least one 128-bit lane group).
            return unsafe { l2_distance_sq_neon(a, b) };
        }
    }
    l2_distance_sq_scalar(a, b)
}

pub(crate) fn l2_distance_sq_scalar(a: &[f32], b: &[f32]) -> f32 {
    let mut sum: f32 = 0.0;
    for (x, y) in a.iter().zip(b.iter()) {
        let d = *x - *y;
        sum += d * d;
    }
    sum
}

#[cfg(target_arch = "aarch64")]
#[target_feature(enable = "neon")]
#[allow(clippy::many_single_char_names)] // NEON intrinsics work in single-letter regs by convention
pub(crate) unsafe fn l2_distance_sq_neon(a: &[f32], b: &[f32]) -> f32 {
    use core::arch::aarch64::{
        float32x4_t, vaddq_f32, vaddvq_f32, vdupq_n_f32, vfmaq_f32, vld1q_f32, vsubq_f32,
    };
    unsafe {
        // Two independent accumulator registers so the FMA dependency
        // chain doesn't serialise (each FMA depends on prior FMA).
        // Pre-conditions checked by caller: `a.len() == b.len()`,
        // `a.len() % 4 == 0`, `a.len() >= 4`.
        let zero: float32x4_t = vdupq_n_f32(0.0);
        let mut acc0 = zero;
        let mut acc1 = zero;
        let n = a.len();
        let mut i = 0usize;
        // Process 8 floats per iter when available (two parallel
        // accumulators). Tail of 4 falls into the second loop.
        while i + 8 <= n {
            let d0 = vsubq_f32(vld1q_f32(a.as_ptr().add(i)), vld1q_f32(b.as_ptr().add(i)));
            acc0 = vfmaq_f32(acc0, d0, d0);
            let d1 = vsubq_f32(
                vld1q_f32(a.as_ptr().add(i + 4)),
                vld1q_f32(b.as_ptr().add(i + 4)),
            );
            acc1 = vfmaq_f32(acc1, d1, d1);
            i += 8;
        }
        while i + 4 <= n {
            let d = vsubq_f32(vld1q_f32(a.as_ptr().add(i)), vld1q_f32(b.as_ptr().add(i)));
            acc0 = vfmaq_f32(acc0, d, d);
            i += 4;
        }
        vaddvq_f32(vaddq_f32(acc0, acc1))
    }
}

/// Public wrapper: run an NSW kNN search and return the top-k row
/// indices ordered by ascending distance under the given metric.
pub fn nsw_query(
    table: &Table,
    idx_name: &str,
    query: &[f32],
    k: usize,
    metric: NswMetric,
) -> Vec<usize> {
    let Some(idx_pos) = table.indices.iter().position(|i| i.name == idx_name) else {
        return Vec::new();
    };
    let ef = (k * 2).max(NSW_DEFAULT_M);
    let mut hits = nsw_search(table, idx_pos, query, k, ef, metric);
    hits.truncate(k);
    hits.into_iter().map(|(_, idx)| idx).collect()
}

/// Find any NSW index on a column. Used by the planner to decide
/// whether an `ORDER BY col <-> literal LIMIT k` query can skip the
/// brute-force scan.
pub fn nsw_index_on(table: &Table, column_position: usize) -> Option<&Index> {
    table
        .indices
        .iter()
        .find(|i| i.column_position == column_position && matches!(i.kind, IndexKind::Nsw(_)))
}
