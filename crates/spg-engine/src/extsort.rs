//! v7.37 (round 833) — T35 Phase B: sort more rows than fit in `work_mem`.
//!
//! Sorting held every row twice. The scan materialises `Vec<Row>`, and
//! the sort then builds `Vec<(Vec<OrderKey>, Row)>` beside it, so peak
//! tracked the input with no ceiling anywhere: measured on
//! `SELECT pad FROM big ORDER BY id`, peak RSS 218 MB at 100k rows,
//! 390 MB at 200k, 807 MB at 400k. `work_mem` reads 4 MB and bounded
//! none of it. A large enough ORDER BY takes the server down, which is
//! a liveness problem rather than a performance one.
//!
//! Round 786/787 left the seam for this — `TempRun` is a byte stream the
//! host provides (a file, on the server) — and nothing ever called
//! `can_spill()`. This is the piece that does: fill up to the budget,
//! sort that much, write it out as a sorted RUN, drop it, repeat; then
//! merge the runs back k-way. Peak is then the budget plus one decoded
//! row per run, whatever the input size.
//!
//! A spilled record is the caller's SOURCE row, under the scan's own
//! schema. Keys come back through `build_order_keys_bound` over that
//! row — the identical call the scan makes — so no key can be missing,
//! and the projection runs once after the merge instead of before the
//! spill. `push` takes `(keys, record)`; `finish` takes `keys_of` and
//! `project`.
//!
//! Two earlier shapes were tried and are recorded because each looked
//! right until it met the call site. Round 833 stored rows alone and
//! re-derived keys from the decoded row: impossible, because the rows
//! were PROJECTED and an ORDER BY key need not be projected (`SELECT
//! pad FROM big ORDER BY id`). Its three unit tests passed anyway,
//! their key having happened to be a projected column — the case that
//! exposes it leads the tests now. Round 835 then spilled key values as
//! leading hidden columns, which `encode_row_body_dense` cannot encode
//! in general: it is schema-driven, an ORDER BY operand is an arbitrary
//! expression, and the tree has no expression type inference to ask for
//! its `DataType`. Spilling the source row removes both problems rather
//! than working around either.
//!
//! The costs are honest ones: a wide source row spills more bytes than
//! a narrow projection would (I/O, not peak memory, which the budget
//! bounds either way), and a projection containing a correlated
//! subquery evaluates after the merge rather than during the scan.
//!
//! With no factory (an embedded engine with nowhere to put a file) this
//! degrades to exactly today's behaviour: everything stays in the batch
//! and is sorted in memory. That is a ceiling the host opts into, not
//! one imposed on it.

// Phase B's core. `try_spill_sorted_scan`, which collects the answer
// into a `QueryResult::Rows`, is still NOT on any query path — it was
// written and measured and held on the perf red line. What IS hooked, as
// of round 882, is `try_spill_sorted_stream`: the same bounded sort
// emitting through the streaming executor, which is what `finish_each`
// below exists to serve.
//
// Numbers, all taken under `docs/BENCH_PROTOCOL.md` (same psql on both
// sides, a non-indexed sort key so PG really sorts, row counts verified,
// `Sort Method: external merge  Disk: 85384kB` confirmed in the plan,
// six interleaved runs with the starting side flipped), 400k rows of
// 200 bytes at work_mem = 4MB:
//
//   PG18                    0.40 - 0.41 s
//   SPG, sort in memory     0.31 - 0.33 s   (faster than PG)
//   SPG, sort spilled       0.46 - 0.51 s   (1.15 - 1.28x slower)
//
// The ranges do not overlap, so the spill's cost is real here — about
// +0.15 s — unlike the earlier readings this replaces. Peak RSS over the
// same run falls from 807 MB to 129 MB, and eight ORDER BY shapes give
// byte-identical answers spilled and unspilled.
//
// Wiring THAT would turn an endpoint SPG currently WINS into one it
// loses, which is a hard stop whatever the memory buys. What round 882
// hooked instead is the streaming twin, and its account is different —
// four binaries in one interleaved run, quiet machine (load 1.6 - 1.8),
// six rounds with the order rotating, same 400k x 200-byte table and
// `work_mem = 4MB` on every side, PG's plan confirmed `external merge`:
//
//   SPG materialising (what shipped)     219.6 - 237.8 ms
//   SPG spilled + streaming              240.8 - 252.9 ms
//   SPG spilled + streaming + reused buf 233.4 - 243.3 ms
//   PG18                                 201.8 - 293.7 ms
//
// Streaming alone cost about 20 ms against the materialising path, and
// the ranges did not overlap, so that cost was real. Reusing one
// projection buffer instead of building a `Vec` per row gave back about
// 7 ms of it and brings the ranges back into overlap; the median is
// still ~12 ms above. Against PG the ranges overlap in both windows
// measured, which under `BENCH_PROTOCOL.md` rule 4 is no measured
// difference — PG's spread is wide here (201 - 294) where SPG's is tight.
//
// What the streaming buys, measured the same way (RSS above the server's
// own baseline, sampled while the query runs, spill runs counted DURING
// it):
//
//              100k rows      200k rows      400k rows
//   shipping   +56 MB (0)     +133 MB (0)    +253 MB (0 runs)
//   streaming  +12 MB (6)     +16 MB (17)    +26 MB (33 runs)
//
// The shipping path never spills at all — that is why it is linear. The
// streaming one is flat to within its run buffers (k x 256 KiB), which
// is the whole of the growth left in it.
//
// Round 883 then took the ~12 ms that was still between this and the
// materialising path, and rather more. A build that pushed a null
// payload of the same arity — same keys, same record count, same merge —
// ran 93 ms faster, and ablation split that three ways:
//
//   projection + 80 MB of wire   ~16 ms   `SELECT pad` vs `SELECT id`,
//                                          same sort, same spill
//   the whole spill round trip   <=15 ms  work_mem 4MB vs 4GB, runs
//                                          witnessed 33 vs 0, RANGES
//                                          OVERLAP so this is a ceiling
//   the per-row clone            ~62 ms   what is left
//
// 16 + 15 + 62 = 93, which is the check. So the batch became an arena:
// rows encoded back to back into one buffer as the scan hands them over
// (borrowed, never cloned into an owned `Row`), keys flattened beside
// them at a fixed stride, and a permutation sorted instead of the rows.
// Spilling writes the arena out in that order and re-encodes nothing;
// the budget meter is `arena.len()` rather than a per-row walk over the
// values. Freeing is one buffer, not 400k.
//
// Same protocol, quiet machine, six rounds, starting side flipped:
//
//   round 882 (clone, streaming)   234.2 - 245.9 ms
//   round 883 (arena)              177.5 - 184.5 ms
//   PG18                           201.8 - 293.7 ms
//
// The arena's range sits entirely below PG's, so this endpoint is faster
// than PG18 rather than level with it. The estimate that predicted it
// (240 - 62 = 178) is the first in this stretch to survive measurement;
// the five before it were all refuted, and the difference is that this
// one was three separately checkable quantities rather than one guess.
//
// What it costs: the in-memory case now round-trips through the codec
// where it used to keep `Row`s, so any value the dense encoding cannot
// carry faithfully would show up in sorts that never spill, not only in
// ones that do. That is the risk the gates are covering, not an argument
// that the codec is complete.
//
// Where the cost is NOT, each measured rather than argued:
//
//   the codec, and the k-way merge's O(n*k) head scan       round 839
//   the row clone into the sorter (16 ms), the extra pass   round 843
//     over the rows (15 ms)
//   the number of runs — 29 runs merge no slower than 2,    round 844
//     so a heap or loser tree buys nothing
//   the merge's per-row allocation — removing two of them   round 845
//     per row, 800k in all, changed nothing measurable
//
// Seven guesses, seven refutations. What is left of the sorter's 186 ms
// is decoding a row and cloning it back out, which is the work spilling
// exists to do; the honest reading is that this is not a hot spot with a
// mistake in it. The comparison worth closing is server-side and smaller
// than the wall clock suggests: 224 ms of spill machinery against PG's
// 152 ms.
//
// Round 880 put a profiler on it, and that last reading was wrong.
// Sampling the walk under load, by leaf symbol: the ALLOCATOR takes 586
// samples (malloc/free/realloc/zone), more than every sort comparison
// combined (420: `order_key_elem_cmp` 154, quicksort 128, `sift_down`
// 59, `head_cmp` 42, insert_tail 37), against 19 for `push` itself. The
// cost is not the codec and not the merge — it is that each row is
// allocated and freed individually, roughly three of each per row, where
// PG palloc's into a context and frees it in one go.
//
// Two of the eight rounds that reached that conclusion were measuring
// nothing, and both failures looked like data:
//
//   * Timers bracketing `approx_values_bytes` and `Vec::push` inside
//     `push` reported 7 and 9 ms against 116 for the body they sit in.
//     `Instant::now()` does not stop LLVM sinking a store past it, so
//     they timed an emptied position. Ablation — delete the step, time
//     the whole — cannot be defeated that way and is what round 877
//     onward used.
//   * The probe counted spill files after the query, and `FileRun::drop`
//     removes each one. It read 0 whatever happened. Counting DURING the
//     query shows 9 / 17 / 33 runs at 100k / 200k / 400k rows: the spill
//     had been working the whole time, and "work_mem is ineffective"
//     (round 873) was that blind witness talking.
#![allow(dead_code)]

use alloc::boxed::Box;
use alloc::vec::Vec;

use spg_storage::{Row, TableSchema, Value};

use crate::EngineError;
use crate::orderby::{OrderKey, cmp_multi_key_in};
use crate::tempstore::TempRun;

/// One sorted run on the host's temp storage, plus the read cursor the
/// merge walks it with.
/// The record at a run's head, and its keys. `None` once drained.
type Head = Option<(Vec<OrderKey>, Row<'static>)>;

/// A slot in the tournament that holds no run.
const NO_RUN: usize = usize::MAX;

/// Order two runs by their heads, with everything that cannot supply a
/// row sorting last: a drained run, then an empty slot.
fn head_cmp(
    heads: &[Head],
    descs: &[bool],
    collations: &[Option<crate::collate::Collated>],
    a: usize,
    b: usize,
) -> core::cmp::Ordering {
    use core::cmp::Ordering;
    match (a == NO_RUN, b == NO_RUN) {
        (true, true) => return Ordering::Equal,
        (true, false) => return Ordering::Greater,
        (false, true) => return Ordering::Less,
        (false, false) => {}
    }
    match (&heads[a], &heads[b]) {
        (None, None) => Ordering::Equal,
        (None, Some(_)) => Ordering::Greater,
        (Some(_), None) => Ordering::Less,
        // v7.38.21 — `.then(a.cmp(&b))`, and the batch sort's promise is
        // why.
        //
        // It says equal keys keep the order they arrived in, and inside a
        // batch they do. A run holds rows that were all pushed before
        // every row of the next run, so preferring the LOWER run index on
        // a tie is that same order continued across the merge. Without
        // it, a binary heap hands back whichever equal head happened to
        // sit higher, and the same ORDER BY answers differently depending
        // on whether the sort spilled -- the exact split `inline_int_key`
        // exists to prevent between its two callers.
        //
        // Not reversed under DESC: arrival order is not a key.
        (Some((ka, _)), Some((kb, _))) => {
            cmp_multi_key_in(ka, kb, descs, collations).then(a.cmp(&b))
        }
    }
}

/// A binary min-heap over the run indices, ordered by their heads.
///
/// Picking the next row used to scan every head, which is `k - 1`
/// comparisons per output row. Round 853 priced that at the run count a
/// 4 MB budget actually produces — 400k rows over 29 runs is 11.2M
/// comparisons and 30.4 ms — against 2.0M and 4.2 ms for an O(log k)
/// structure.
///
/// Round 844 had concluded such a structure was not worth having, from
/// 29 runs merging no slower than 2. That reading was confounded: the
/// 2-run case gives `finish` a 95k-row final batch to sort in memory
/// before it merges anything, which is most of what it measured.
///
/// A heap rather than a loser tree: the tree needs one comparison per
/// level to the heap's two, but its construction has an ordering
/// subtlety that a first attempt here got wrong (each replay overwrote
/// the previous winner, and the merge returned a single row). The heap
/// is the structure whose correctness is easy to keep, and it captures
/// the same order of the win.
struct HeadHeap {
    /// Run indices, `heap[0]` being the one whose head sorts first.
    heap: Vec<usize>,
}

impl HeadHeap {
    fn build(
        k: usize,
        heads: &[Head],
        descs: &[bool],
        collations: &[Option<crate::collate::Collated>],
    ) -> Self {
        let mut h = Self {
            heap: (0..k).filter(|&i| heads[i].is_some()).collect(),
        };
        for start in (0..h.heap.len() / 2).rev() {
            h.sift_down(start, heads, descs, collations);
        }
        h
    }

    fn peek(&self) -> Option<usize> {
        self.heap.first().copied()
    }

    /// The run at the top has a new head — or none left, in which case it
    /// leaves the heap.
    fn settle_root(
        &mut self,
        heads: &[Head],
        descs: &[bool],
        collations: &[Option<crate::collate::Collated>],
    ) {
        let Some(&top) = self.heap.first() else {
            return;
        };
        if heads[top].is_none() {
            let last = self.heap.pop();
            if let Some(last) = last
                && !self.heap.is_empty()
            {
                self.heap[0] = last;
            }
        }
        if !self.heap.is_empty() {
            self.sift_down(0, heads, descs, collations);
        }
    }

    fn sift_down(
        &mut self,
        mut node: usize,
        heads: &[Head],
        descs: &[bool],
        collations: &[Option<crate::collate::Collated>],
    ) {
        let n = self.heap.len();
        loop {
            let (l, r) = (2 * node + 1, 2 * node + 2);
            let mut best = node;
            if l < n
                && head_cmp(heads, descs, collations, self.heap[l], self.heap[best])
                    == core::cmp::Ordering::Less
            {
                best = l;
            }
            if r < n
                && head_cmp(heads, descs, collations, self.heap[r], self.heap[best])
                    == core::cmp::Ordering::Less
            {
                best = r;
            }
            if best == node {
                return;
            }
            self.heap.swap(node, best);
            node = best;
        }
    }
}

/// Read exactly `n` bytes into `buf`, or `None` at a clean end of stream.
///
/// The buffer is the caller's and is reused across rows. It used to be a
/// fresh `Vec` per call, which the merge makes twice per row — once for a
/// FOUR-BYTE length prefix — and r1032 counted that as half of the external
/// sorter's four allocations per row.
fn read_exact(
    run: &mut dyn TempRun,
    n: usize,
    buf: &mut Vec<u8>,
) -> Result<Option<()>, EngineError> {
    buf.clear();
    buf.resize(n, 0u8);
    let mut filled = 0;
    while filled < n {
        let got = run
            .read(&mut buf[filled..])
            .map_err(|e| EngineError::Internal(alloc::format!("temp run read: {e:?}")))?;
        if got == 0 {
            if filled == 0 {
                return Ok(None);
            }
            return Err(EngineError::Internal(alloc::string::String::from(
                "temp run ended mid-record",
            )));
        }
        filled += got;
    }
    Ok(Some(()))
}

/// Accumulate rows, spilling sorted runs once the budget is reached.
pub(crate) struct ExternalSorter<'a> {
    factory: Option<crate::TempRunFactory>,
    budget_bytes: usize,
    /// The schema of the RECORD a run stores — the caller's source row,
    /// whose types are already declared.
    record_schema: TableSchema,
    descs: &'a [bool],
    /// v7.38.22 — the collation for each key position, resolved once by
    /// the caller.
    ///
    /// This sorter compared with `cmp_multi_key_in(.., &[])` at every
    /// site, so an ORDER BY that named a collation — or a database that
    /// declared one — was ordered by BYTES here while the materialising
    /// sort honoured it. Same query, two answers, decided by which path
    /// the planner took, and the byte one is silently wrong:
    /// PostgreSQL 18.4 answers `apple, Banana, Cherry, date` for
    /// `ORDER BY s COLLATE "en_US.utf8"` and every published SPG through
    /// 7.38.21 answered `Banana, Cherry, apple, date`.
    collations: &'a [Option<crate::collate::Collated>],
    /// The batch, held the way PG holds one: the rows encoded back to
    /// back in ONE buffer, the keys flattened beside them, and nothing
    /// allocated per row.
    ///
    /// It used to be `Vec<(Vec<OrderKey>, Row<'static>)>`, which cost an
    /// owned deep copy of every row on the way in — a `Vec` and a
    /// `String` per text cell — and freed them one at a time on the way
    /// out. Round 883 priced that clone at ~62 ms of a ~240 ms 400k-row
    /// sort by ablation, against ~16 ms for the projection and the 80 MB
    /// of wire and ~15 ms for the whole spill round trip.
    arena: Vec<u8>,
    /// Where row `i`'s body ENDS in `arena`; it starts at `ends[i - 1]`,
    /// or 0 for the first. Byte offsets rather than slices so the arena
    /// can grow underneath them.
    ends: Vec<u32>,
    /// Row `i`'s sort keys are `keys[i * key_stride ..][.. key_stride]`.
    keys: Vec<OrderKey>,
    /// Fixed by the first push: every row of one sort has the same
    /// number of keys, because they come from the same ORDER BY.
    key_stride: usize,
    runs: Vec<Box<dyn TempRun>>,
    /// Where a spill reports itself, when the host wants to know.
    stats: Option<&'a crate::tempstore::SpillStats>,
    /// Which of the record's columns the caller will actually read, so
    /// the rest are never stored. `&[]` stores every column.
    ///
    /// It lives on the sorter rather than being handed to `push` and to
    /// `finish_each` separately BECAUSE those two must agree: a column
    /// stored as null and then decoded as present would read whatever
    /// bytes followed it. One field, one mask, used by both — the
    /// mismatch cannot be written.
    needed: &'a [bool],
}

impl<'a> ExternalSorter<'a> {
    /// `record_cols` describes the rows handed to `push` — the scan's
    /// own source columns, not the projection. Sorting works on records
    /// that still carry every column the ORDER BY might name.
    pub(crate) fn new(
        factory: Option<crate::TempRunFactory>,
        budget_bytes: usize,
        record_cols: Vec<spg_storage::ColumnSchema>,
        descs: &'a [bool],
        collations: &'a [Option<crate::collate::Collated>],
    ) -> Self {
        Self {
            factory,
            budget_bytes,
            record_schema: TableSchema::new("spg_sort_run", record_cols),
            descs,
            collations,
            arena: Vec::new(),
            ends: Vec::new(),
            keys: Vec::new(),
            key_stride: 0,
            runs: Vec::new(),
            stats: None,
            needed: &[],
        }
    }

    /// Store only the columns `needed` marks, as round 995 measured the
    /// need for: the sort was carrying 215 bytes of payload per row for a
    /// query that read eight of them, and spilling all of it.
    ///
    /// The mask must be the one the caller's projection and ORDER BY were
    /// proved against (`sort_record_columns_needed`); a column outside it
    /// reads NULL, silently. Empty is the unpruned sort.
    pub(crate) fn with_pruned(mut self, needed: &'a [bool]) -> Self {
        debug_assert!(
            needed.is_empty() || needed.len() == self.record_schema.columns.len(),
            "prune mask must match the record arity"
        );
        self.needed = needed;
        self
    }

    /// Did this sort actually spill? Pins read it; so does the decision
    /// to report temp bytes.
    pub(crate) fn spilled(&self) -> bool {
        !self.runs.is_empty()
    }

    /// `record` is the row the caller wants back in order — the source
    /// row, so a spilled record can always be re-keyed. It is BORROWED:
    /// the batch keeps its encoded bytes, not the row.
    ///
    /// `keys` is drained rather than consumed, so one caller-owned `Vec`
    /// serves every row and keeps its capacity.
    pub(crate) fn push(
        &mut self,
        keys: &mut Vec<OrderKey>,
        record: &Row<'_>,
    ) -> Result<(), EngineError> {
        if self.ends.is_empty() {
            self.key_stride = keys.len();
        }
        debug_assert_eq!(
            keys.len(),
            self.key_stride,
            "every row of one sort carries the same number of keys"
        );
        self.keys.append(keys);
        spg_storage::encode_row_body_dense_masked_into(
            record,
            &self.record_schema,
            self.needed,
            &mut self.arena,
        );
        let end = u32::try_from(self.arena.len()).map_err(|_| {
            EngineError::Internal(alloc::string::String::from("sort batch larger than 4 GiB"))
        })?;
        self.ends.push(end);
        // Spilling needs somewhere to spill to. Without a factory this
        // is the old unbounded behaviour, deliberately.
        //
        // The meter is exact and O(1) now: what the batch holds IS the
        // arena, where it used to be a per-row walk over the values
        // estimating what their `String`s cost.
        if self.factory.is_some() && self.batch_bytes() >= self.budget_bytes {
            self.spill_batch()?;
        }
        Ok(())
    }

    /// Report every run this sorter writes into `stats`.
    ///
    /// Optional so the unit tests and the embedded engine keep the
    /// four-argument constructor; the server passes its counters.
    pub(crate) fn with_stats(mut self, stats: &'a crate::tempstore::SpillStats) -> Self {
        self.stats = Some(stats);
        self
    }

    /// What the batch is holding, in bytes.
    fn batch_bytes(&self) -> usize {
        self.arena.len()
            + self.keys.len() * core::mem::size_of::<OrderKey>()
            + self.ends.len() * core::mem::size_of::<u32>()
    }

    /// Row `i`'s body, as a span into `arena`.
    fn span(&self, i: usize) -> (usize, usize) {
        let start = if i == 0 { 0 } else { self.ends[i - 1] as usize };
        (start, self.ends[i] as usize)
    }

    /// The batch's row indices, in sorted order.
    ///
    /// A permutation rather than a sort of the rows themselves: the rows
    /// are bytes in one buffer and moving them would cost more than the
    /// indirection saves.
    ///
    /// A single integer key carries the key WITH the index instead. The
    /// general path below sorts `u32`s through a comparator that reads
    /// two `OrderKey`s out of a separate array: the reads follow the
    /// permutation rather than memory, and each comparison of two
    /// integers pays two scattered loads, a slice, and the enum match.
    /// Round 935's leaf-symbol profile put 27.9% of a 10k `SELECT id
    /// FROM t ORDER BY k` in that machinery — more, on its own, than
    /// PG18 spends on the whole sort — while an earlier ablation had
    /// priced it at zero. The ablation was read under a baseline spread
    /// wide enough to hide the difference; the profile is what
    /// contradicted it.
    ///
    /// Measured, 400k rows at `work_mem = 4 MB` so the sort spills, six
    /// interleaved rounds with the starting side flipped, min of three
    /// per round, server-side `EXPLAIN ANALYZE`, and a same-binary
    /// control leg in the same run to show the harness reports no
    /// difference where there is none:
    ///
    ///   SELECT id  FROM t ORDER BY k    120.8 - 123.0  ->  107.8 - 110.3 ms
    ///   SELECT pad FROM t ORDER BY k    126.3 - 128.7  ->  114.0 - 116.4 ms
    ///
    /// Neither pair of ranges overlaps, the control leg's does overlap
    /// the baseline's, and all three legs answer with the same checksum
    /// ascending and descending. Those are the numbers for THIS code; a
    /// prototype without the range guard below measured the same two
    /// shapes at -10.8% and -10.0%, so the guard costs nothing readable.
    /// The 10k shape came out VOID under the spread rule all three times
    /// it was run — 5 ms is below what this machine resolves — so it is
    /// not claimed either way.
    fn sorted_order(&self) -> Vec<u32> {
        let mut order: Vec<u32> = (0..self.ends.len() as u32).collect();
        if self.key_stride == 0 {
            return order;
        }
        let (keys, stride, descs) = (&self.keys, self.key_stride, self.descs);
        let collations = self.collations;
        // v7.38.22 — ONE inline path, and it takes text as well as
        // integers.
        //
        // There were two: `stride == 1` with an exact integer key, and
        // `stride >= 2` with an integer FIRST key and the later keys
        // settling its runs. Neither took text, so a spilled
        // `ORDER BY <text>` compared full strings on every comparison —
        // and under a declared collation, that is a full ICU comparison
        // every time.
        //
        // The panel could not see it until v7.38.22 added a cell for a
        // text sort that RETURNS its rows, which is the shape that takes
        // this sorter. It read 561.8 ms against 216.1 ms for the same
        // query under `C` — **2.60x for declaring a collation** — while
        // the materialising sort, which got the abbreviated key earlier
        // in this version, read 1.30x for the same thing.
        //
        // Third time this release that a capability existed on one path
        // and not its twin. The rule is `orderby::inline_sort_key`'s,
        // shared, so the two cannot drift.
        if let Some((mut inline, exact)) = Self::inline_first_keys(keys, stride, collations) {
            if descs.first().copied().unwrap_or(false) {
                inline.sort_by_key(|p| core::cmp::Reverse(p.0));
            } else {
                inline.sort_by_key(|p| p.0);
            }
            // A prefix key decides only where it differs, so every run of
            // equal keys still goes to the full comparator — the same
            // pass a second ORDER BY key already needed. An exact key
            // with one term needs neither.
            if stride > 1 || !exact {
                let mut lo = 0;
                while lo < inline.len() {
                    let mut hi = lo + 1;
                    while hi < inline.len() && inline[hi].0 == inline[lo].0 {
                        hi += 1;
                    }
                    if hi - lo > 1 {
                        inline[lo..hi].sort_by(|&(_, a), &(_, b)| {
                            let (a, b) = (a as usize * stride, b as usize * stride);
                            cmp_multi_key_in(
                                &keys[a..a + stride],
                                &keys[b..b + stride],
                                descs,
                                collations,
                            )
                        });
                    }
                    lo = hi;
                }
            }
            for (slot, (_, i)) in order.iter_mut().zip(inline) {
                *slot = i;
            }
            return order;
        }
        order.sort_by(|&a, &b| {
            let (a, b) = (a as usize * stride, b as usize * stride);
            cmp_multi_key_in(
                &keys[a..a + stride],
                &keys[b..b + stride],
                descs,
                collations,
            )
        });
        order
    }

    /// `(first key, row index)` when EVERY row's first key travels as an
    /// integer, or `None` when one does not.
    ///
    /// Only the first: the later keys stay `OrderKey`s and are compared
    /// by the general comparator inside a run, so a text or numeric
    /// second key costs this path nothing.
    fn inline_first_keys(
        keys: &[OrderKey],
        stride: usize,
        collations: &[Option<crate::collate::Collated>],
    ) -> Option<(Vec<(i128, u32)>, bool)> {
        let collated = collations.iter().any(Option::is_some);
        if collated
            && !collations
                .iter()
                .flatten()
                .all(crate::collate::Collated::ascii_byte_order)
        {
            return None;
        }
        let rows = keys.len() / stride;
        let mut out: Vec<(i128, u32)> = Vec::with_capacity(rows);
        let mut exact = true;
        let (mut saw_int, mut saw_text) = (false, false);
        for i in 0..rows {
            let k = &keys[i * stride];
            let (v, is_exact, byte_ordered) = crate::orderby::inline_sort_key(k)?;
            if collated && !byte_ordered {
                return None;
            }
            match k {
                OrderKey::Text(_) => saw_text = true,
                OrderKey::Int(_) => saw_int = true,
                _ => {}
            }
            exact &= is_exact;
            out.push((v, i as u32));
        }
        if saw_int && saw_text {
            return None;
        }
        Some((out, exact))
    }

    /// `(key, row index)` for one integer key per row, or `None` when
    /// some key is not one — every other key type sorts the general way.
    ///
    /// Which keys qualify is [`crate::orderby::inline_int_key`]'s to say,
    /// and only its: the materialising sort asks the same question, and
    /// two answers to it would make one ORDER BY depend on which path the
    /// query took.
    fn inline_int_keys(keys: &[OrderKey]) -> Option<Vec<(i128, u32)>> {
        let mut out: Vec<(i128, u32)> = Vec::with_capacity(keys.len());
        for (i, k) in keys.iter().enumerate() {
            out.push((crate::orderby::inline_int_key(k)?, i as u32));
        }
        Some(out)
    }

    fn clear_batch(&mut self) {
        self.arena.clear();
        self.ends.clear();
        self.keys.clear();
    }

    /// Sort what is held and write it out as one run, freeing the rows.
    fn spill_batch(&mut self) -> Result<(), EngineError> {
        if self.ends.is_empty() {
            return Ok(());
        }
        let Some(factory) = self.factory else {
            return Ok(());
        };
        let order = self.sorted_order();
        let mut run = factory()
            .map_err(|e| EngineError::Internal(alloc::format!("temp run create: {e:?}")))?;
        // The bytes are already encoded — a run is the arena written out
        // in sorted order, so spilling re-encodes nothing.
        for i in order {
            let (start, end) = self.span(i as usize);
            let len = u32::try_from(end - start).map_err(|_| {
                EngineError::Internal(alloc::string::String::from("row too large to spill"))
            })?;
            run.append(&len.to_le_bytes())
                .map_err(|e| EngineError::Internal(alloc::format!("temp run append: {e:?}")))?;
            run.append(&self.arena[start..end])
                .map_err(|e| EngineError::Internal(alloc::format!("temp run append: {e:?}")))?;
        }
        run.seal()
            .map_err(|e| EngineError::Internal(alloc::format!("temp run seal: {e:?}")))?;
        // After seal, so the figure is what the run really holds.
        if let Some(stats) = self.stats {
            use core::sync::atomic::Ordering;
            stats.files.fetch_add(1, Ordering::Relaxed);
            stats
                .bytes
                .fetch_add(run.bytes_written(), Ordering::Relaxed);
        }
        self.runs.push(run);
        self.clear_batch();
        Ok(())
    }

    /// Every row, in order. `keys_of` re-derives a spilled row's keys —
    /// the same function the caller used on the way in, so a run needs
    /// to carry only the row.
    /// Every row, in order, projected.
    ///
    /// `keys_of` re-derives a spilled record's keys — the caller passes
    /// the same call the scan used, over the same source row, so no key
    /// can be missing. `project` turns a record into the output row, and
    /// runs once per row whether or not the sort spilled.
    /// Sorted rows, one at a time, into `emit` — the shape that bounds
    /// peak memory.
    ///
    /// `finish` holds the whole answer: the budget bounds what the SORT
    /// costs, but the merged output is collected into one
    /// `Vec<Row<'static>>` and the caller's `QueryResult::Rows` keeps it,
    /// so peak still tracks the RESULT. Measured over the spilled walk at
    /// `work_mem = 4 MB`, 200-byte rows, RSS above the server's own
    /// baseline while the query runs: +30 MB at 100k rows, +68 MB at
    /// 200k, +137 MB at 400k — linear, with the spill working correctly
    /// underneath it (9 / 17 / 33 runs). Round 873 read that growth as
    /// `work_mem` being ineffective; the runs say otherwise, and the
    /// growth is the collected result.
    ///
    /// Handing each row over instead makes peak the budget plus one run
    /// buffer per run plus a single row, whatever the answer's size —
    /// which is what a merge already has in hand at every step. The row
    /// is passed BY VALUE so `finish` can wrap this without a clone.
    ///
    /// Returns how many rows were emitted.
    /// `needed[i]` false means neither the projection nor the ORDER BY
    /// (which the merge re-keys from a decoded row) reads column `i`, so
    /// the decode may walk past it. `&[]` reads everything.
    pub(crate) fn finish_each<K, P, E>(
        mut self,
        keys_of: K,
        project: P,
        mut emit: E,
    ) -> Result<usize, EngineError>
    where
        K: Fn(&Row<'static>, &mut Vec<OrderKey>) -> Result<(), EngineError>,
        P: Fn(&Row<'static>, &mut Vec<Value<'static>>) -> Result<(), EngineError>,
        E: FnMut(&[Value<'static>]) -> Result<(), EngineError>,
    {
        let mut emitted = 0usize;
        // The SAME mask `push` stored with: see the field's note.
        let needed = self.needed;
        // One buffer for every output row. Building a fresh `Vec` per row
        // was 400k allocations on the 400k-row sort, and the allocator is
        // where this walk's time goes (round 880's profile: 586 samples
        // against 420 for every sort comparison combined). `emit` only
        // borrows, so the buffer can be refilled rather than rebuilt --
        // the same reason PG projects through one reused slot.
        let mut scratch: Vec<Value<'static>> = Vec::new();
        if self.runs.is_empty() {
            // Never spilled: sort the batch where it lies and decode each
            // row as it is handed over.
            for i in self.sorted_order() {
                let (start, end) = self.span(i as usize);
                let (row, _) = spg_storage::decode_row_body_dense_pruned(
                    &self.arena[start..end],
                    &self.record_schema,
                    spg_storage::CURRENT_ROW_CODEC_VERSION,
                    needed,
                )
                .map_err(|e| EngineError::Internal(alloc::format!("sort batch decode: {e:?}")))?;
                scratch.clear();
                project(&row, &mut scratch)?;
                emit(&scratch)?;
                emitted += 1;
            }
            return Ok(emitted);
        }
        // Whatever is still held becomes the last run, so the merge has
        // one kind of input rather than two.
        self.spill_batch()?;

        // Runs and their heads live side by side rather than in one
        // struct: the tournament reads every head while one run is being
        // advanced, which a single `Vec<RunReader>` would not allow.
        let mut runs: Vec<Box<dyn TempRun>> = core::mem::take(&mut self.runs);
        let mut heads: Vec<Head> = Vec::with_capacity(runs.len());
        // One read buffer for the whole merge. Every row of every run
        // borrows it and hands it straight to the decoder, so it grows
        // once to the widest row and then stops allocating.
        let mut readbuf: Vec<u8> = Vec::new();
        for run in &mut runs {
            heads.push(Self::next_row(
                &mut **run,
                &self.record_schema,
                &keys_of,
                needed,
                &mut readbuf,
                Vec::new(),
            )?);
        }

        let mut heap = HeadHeap::build(heads.len(), &heads, self.descs, self.collations);
        while let Some(w) = heap.peek() {
            let Some((keys, record)) = heads[w].take() else {
                break;
            };
            scratch.clear();
            project(&record, &mut scratch)?;
            emit(&scratch)?;
            emitted += 1;
            heads[w] = Self::next_row(
                &mut *runs[w],
                &self.record_schema,
                &keys_of,
                needed,
                &mut readbuf,
                keys,
            )?;
            heap.settle_root(&heads, self.descs, self.collations);
        }
        Ok(emitted)
    }

    pub(crate) fn finish<K, P>(
        self,
        keys_of: K,
        project: P,
    ) -> Result<Vec<Row<'static>>, EngineError>
    where
        K: Fn(&Row<'static>, &mut Vec<OrderKey>) -> Result<(), EngineError>,
        P: Fn(&Row<'static>) -> Result<Row<'static>, EngineError>,
    {
        let mut out: Vec<Row<'static>> = Vec::new();
        self.finish_each(
            keys_of,
            |src, buf| {
                buf.extend(project(src)?.values);
                Ok(())
            },
            |cells| {
                out.push(Row::new(cells.to_vec()));
                Ok(())
            },
        )?;
        Ok(out)
    }

    /// `keys_of` receives the decoded SOURCE row, which still carries
    /// every column an ORDER BY could name.
    fn next_row<K>(
        run: &mut dyn TempRun,
        schema: &TableSchema,
        keys_of: &K,
        needed: &[bool],
        // Reused across every row of every run: see `read_exact`.
        buf: &mut Vec<u8>,
        // The key buffer of the head this one replaces. Only `runs.len()`
        // heads are alive at once, so recycling theirs means the merge
        // allocates key storage that many times rather than once per row.
        mut keys: Vec<OrderKey>,
    ) -> Result<Option<(Vec<OrderKey>, Row<'static>)>, EngineError>
    where
        K: Fn(&Row<'static>, &mut Vec<OrderKey>) -> Result<(), EngineError>,
    {
        if read_exact(run, 4, buf)?.is_none() {
            return Ok(None);
        }
        let len = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
        let body = buf;
        if read_exact(run, len as usize, body)?.is_none() {
            return Err(EngineError::Internal(alloc::string::String::from(
                "temp run ended before its row body",
            )));
        }
        let (row, _) = spg_storage::decode_row_body_dense_pruned(
            body,
            schema,
            spg_storage::CURRENT_ROW_CODEC_VERSION,
            needed,
        )
        .map_err(|e| EngineError::Internal(alloc::format!("temp run row decode: {e:?}")))?;
        keys.clear();
        keys_of(&row, &mut keys)?;
        Ok(Some((keys, row)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::string::ToString;
    use spg_storage::{ColumnSchema, DataType, Value};

    /// A run backed by a Vec, so the sorter's own logic can be tested
    /// without a host that owns files.
    struct MemRun {
        buf: Vec<u8>,
        read_at: usize,
        sealed: bool,
    }

    impl TempRun for MemRun {
        fn append(&mut self, bytes: &[u8]) -> Result<(), crate::tempstore::TempStoreError> {
            assert!(!self.sealed, "append after seal");
            self.buf.extend_from_slice(bytes);
            Ok(())
        }
        fn seal(&mut self) -> Result<(), crate::tempstore::TempStoreError> {
            self.sealed = true;
            self.read_at = 0;
            Ok(())
        }
        fn read(&mut self, buf: &mut [u8]) -> Result<usize, crate::tempstore::TempStoreError> {
            let n = core::cmp::min(buf.len(), self.buf.len() - self.read_at);
            buf[..n].copy_from_slice(&self.buf[self.read_at..self.read_at + n]);
            self.read_at += n;
            Ok(n)
        }
        fn bytes_written(&self) -> u64 {
            self.buf.len() as u64
        }
    }

    fn mem_run() -> Result<Box<dyn TempRun>, crate::tempstore::TempStoreError> {
        Ok(Box::new(MemRun {
            buf: Vec::new(),
            read_at: 0,
            sealed: false,
        }))
    }

    /// The scan's own columns: a spilled record keeps them ALL, so the
    /// ORDER BY can name one the projection drops.
    fn record_cols() -> Vec<ColumnSchema> {
        alloc::vec![
            ColumnSchema::new("id", DataType::Int, false),
            ColumnSchema::new("pad", DataType::Text, false),
        ]
    }

    fn record(id: i32) -> Row<'static> {
        Row::new(alloc::vec![
            Value::Int(id),
            Value::text(alloc::format!("pad-{id}")),
        ])
    }

    /// Pack the key from the SOURCE row, standing in for
    /// `build_order_keys_bound` (which needs an evaluation context).
    /// Appends, like the real one, so the merge can hand it a recycled
    /// buffer.
    fn keys_of(src: &Row<'static>, out: &mut Vec<OrderKey>) -> Result<(), EngineError> {
        match &src.values[0] {
            Value::Int(n) => {
                out.push(OrderKey::Int(i128::from(*n)));
                Ok(())
            }
            other => Err(EngineError::Internal(alloc::format!("bad key: {other:?}"))),
        }
    }

    /// v7.38.22 — the text key, re-derived from the row the way the
    /// pusher derived it.
    fn keys_of_text(src: &Row<'static>, out: &mut Vec<OrderKey>) -> Result<(), EngineError> {
        match &src.values[0] {
            Value::Int(n) => {
                let i = *n;
                out.push(OrderKey::Text(
                    alloc::format!("PFX{}xxxx-{:04}", i % 5, (i * 7919) % 200)
                        .as_str()
                        .into(),
                ));
                Ok(())
            }
            other => Err(EngineError::Internal(alloc::format!("bad key: {other:?}"))),
        }
    }

    /// v7.38.21 — two keys, re-derived from the row the same way the
    /// pusher derived them, which is the contract `finish` states.
    fn keys_of_two(src: &Row<'static>, out: &mut Vec<OrderKey>) -> Result<(), EngineError> {
        match &src.values[0] {
            Value::Int(n) => {
                let id = i128::from(*n);
                out.push(OrderKey::Int(id % 3));
                out.push(OrderKey::Int((id * 7919) % 13));
                Ok(())
            }
            other => Err(EngineError::Internal(alloc::format!("bad key: {other:?}"))),
        }
    }

    /// The same, for the tests that want the keys as a value.
    fn keys_vec(src: &Row<'static>) -> Vec<OrderKey> {
        let mut out = Vec::new();
        keys_of(src, &mut out).expect("test row has an int key");
        out
    }

    fn ids_of(rows: &[Row<'static>], at: usize) -> Vec<i32> {
        rows.iter()
            .map(|r| match r.values[at] {
                Value::Int(n) => n,
                _ => panic!("int column at {at}"),
            })
            .collect()
    }

    /// Project away the key column, so the output carries `pad` alone.
    fn project_pad_only(src: &Row<'static>) -> Result<Row<'static>, EngineError> {
        Ok(Row::new(alloc::vec![src.values[1].clone()]))
    }

    fn project_identity(src: &Row<'static>) -> Result<Row<'static>, EngineError> {
        Ok(src.clone())
    }

    /// r995 — the sort STORES only the columns the caller proved it reads.
    ///
    /// Before this, the prune mask reached only the decode, so a sort of
    /// `SELECT id FROM t ORDER BY k` carried every row's whole payload
    /// into the batch and out to the spill file. Measured against PG18 on
    /// 400k rows of 200-byte payload: SPG wrote 215 bytes per row where PG
    /// wrote 18.1, and the endpoint lost by 30%.
    ///
    /// Both halves are pinned here because only the pair is meaningful:
    /// the pruned batch must be SMALL (or the change did nothing) and the
    /// rows must still come out in the right order carrying the right
    /// values (or it broke the answer). The unpruned sorter beside it is
    /// the control — same rows, same pushes, so a shrunken batch can only
    /// be the mask.
    #[test]
    fn pruned_sort_stores_only_what_the_caller_reads() {
        let descs = [false];
        // Payload wide enough that carrying it is unmistakable.
        let cols = alloc::vec![
            ColumnSchema::new("id", DataType::Int, false),
            ColumnSchema::new("pad", DataType::Text, false),
        ];
        let wide = |id: i32| {
            Row::new(alloc::vec![
                Value::Int(id),
                Value::text(core::iter::repeat_n('x', 200).collect::<alloc::string::String>()),
            ])
        };

        let mut full = ExternalSorter::new(None, usize::MAX, cols.clone(), &descs, &[]);
        // `id` is read, `pad` is not.
        let mask = [true, false];
        let mut lean = ExternalSorter::new(None, usize::MAX, cols, &descs, &[]).with_pruned(&mask);
        for i in 0..1000i32 {
            let r = wide((i * 7919) % 1000);
            let mut k = keys_vec(&r);
            full.push(&mut k, &r).unwrap();
            let mut k = keys_vec(&r);
            lean.push(&mut k, &r).unwrap();
        }

        let (full_bytes, lean_bytes) = (full.arena.len(), lean.arena.len());
        assert!(
            full_bytes > 200 * 1000,
            "control must be carrying the payload, got {full_bytes} B for 1000 rows"
        );
        assert!(
            lean_bytes * 20 < full_bytes,
            "pruned batch {lean_bytes} B is not decisively smaller than {full_bytes} B"
        );

        // ... and the answer is unchanged. The dropped column reads NULL,
        // which is the contract `sort_record_columns_needed` is timid
        // enough to only ever hand out when nothing reads it.
        let mut seen: Vec<i32> = Vec::new();
        let n = lean
            .finish_each(
                keys_of,
                |src, buf| {
                    assert!(
                        matches!(src.values[1], Value::Null),
                        "a pruned column must read NULL, not {:?}",
                        src.values[1]
                    );
                    buf.push(src.values[0].clone());
                    Ok(())
                },
                |cells| {
                    match cells[0] {
                        Value::Int(v) => seen.push(v),
                        ref other => panic!("int expected, got {other:?}"),
                    }
                    Ok(())
                },
            )
            .unwrap();
        assert_eq!(n, 1000);
        assert_eq!(seen, (0..1000i32).collect::<Vec<_>>(), "sorted by id");
    }

    /// r995 — the same pruning on the SPILLED path, where the bytes
    /// actually reach a file.
    ///
    /// The in-memory pin above cannot see this: `finish_each` decodes the
    /// batch in place when nothing spilled. Here the budget forces runs,
    /// so what shrinks is what was written out — the thing the endpoint
    /// measurement was about.
    #[test]
    fn pruned_sort_writes_less_to_its_runs() {
        let descs = [false];
        let cols = alloc::vec![
            ColumnSchema::new("id", DataType::Int, false),
            ColumnSchema::new("pad", DataType::Text, false),
        ];
        let wide = |id: i32| {
            Row::new(alloc::vec![
                Value::Int(id),
                Value::text(core::iter::repeat_n('x', 200).collect::<alloc::string::String>()),
            ])
        };
        let mask = [true, false];

        let run_one = |needed: &[bool]| -> (u64, u64, Vec<i32>) {
            let stats = crate::tempstore::SpillStats::default();
            let mut s = ExternalSorter::new(Some(mem_run), 4096, cols.clone(), &descs, &[])
                .with_stats(&stats)
                .with_pruned(needed);
            for i in 0..2000i32 {
                let r = wide((i * 7919) % 2000);
                let mut k = keys_vec(&r);
                s.push(&mut k, &r).unwrap();
            }
            assert!(s.spilled(), "a 4 KB budget over 2000 wide rows must spill");
            let mut seen: Vec<i32> = Vec::new();
            s.finish_each(
                keys_of,
                |src, buf| {
                    buf.push(src.values[0].clone());
                    Ok(())
                },
                |cells| {
                    match cells[0] {
                        Value::Int(v) => seen.push(v),
                        ref other => panic!("int expected, got {other:?}"),
                    }
                    Ok(())
                },
            )
            .unwrap();
            (
                stats.bytes.load(core::sync::atomic::Ordering::Relaxed),
                stats.files.load(core::sync::atomic::Ordering::Relaxed),
                seen,
            )
        };

        let (full_bytes, full_files, full_ids) = run_one(&[]);
        let (lean_bytes, lean_files, lean_ids) = run_one(&mask);
        assert!(
            lean_bytes * 20 < full_bytes,
            "pruned run wrote {lean_bytes} B against the control's {full_bytes} B"
        );
        assert!(
            lean_files < full_files,
            "fewer bytes must mean fewer runs: {lean_files} against {full_files}"
        );
        assert_eq!(full_ids, lean_ids, "pruning must not change the order");
        assert_eq!(lean_ids, (0..2000i32).collect::<Vec<_>>());
    }

    /// r882 — `finish_each` hands rows over as the merge produces them.
    ///
    /// The property that separates streaming from collecting-then-
    /// replaying is where the work STOPS: an emitter that refuses the
    /// third row must leave the rest unprojected. A `finish` that built
    /// the whole answer first would have projected all 5000 before the
    /// emitter ever saw row one, so the count below pins the shape and
    /// not just the output. Spilled, so the merge is the thing under
    /// test — 5000 rows against a 4 KB budget is many runs.
    #[test]
    fn finish_each_stops_when_the_consumer_stops() {
        use core::cell::Cell;

        let descs = [false];
        let mut s = ExternalSorter::new(Some(mem_run), 4096, record_cols(), &descs, &[]);
        for i in 0..5000i32 {
            let r = record((i * 7919) % 5000);
            let mut k = keys_vec(&r);
            s.push(&mut k, &r).unwrap();
        }

        let projected = Cell::new(0usize);
        let seen = Cell::new(0usize);
        let err = s
            .finish_each(
                keys_of,
                |src, buf| {
                    projected.set(projected.get() + 1);
                    buf.extend(project_identity(src)?.values);
                    Ok(())
                },
                |_cells| {
                    seen.set(seen.get() + 1);
                    if seen.get() == 3 {
                        return Err(EngineError::Internal("consumer stopped".to_string()));
                    }
                    Ok(())
                },
            )
            .unwrap_err();

        assert!(matches!(err, EngineError::Internal(ref m) if m == "consumer stopped"));
        assert_eq!(seen.get(), 3, "the consumer saw exactly three rows");
        assert_eq!(
            projected.get(),
            3,
            "the merge stopped with the consumer; a collecting finish would \
             have projected all 5000 first"
        );
    }

    /// r882 — and it is the same answer `finish` gives, spilled or not.
    #[test]
    fn finish_each_matches_finish_spilled_and_unspilled() {
        let descs = [false];
        for budget in [4096usize, 64 * 1024 * 1024] {
            let mut a = ExternalSorter::new(Some(mem_run), budget, record_cols(), &descs, &[]);
            let mut b = ExternalSorter::new(Some(mem_run), budget, record_cols(), &descs, &[]);
            for i in 0..2000i32 {
                let r = record((i * 7919) % 2000);
                a.push(&mut keys_vec(&r), &r).unwrap();
                b.push(&mut keys_vec(&r), &r).unwrap();
            }
            let spilled = !a.runs.is_empty();
            assert_eq!(
                spilled,
                budget == 4096,
                "budget {budget} should decide whether this spills"
            );

            let collected = a.finish(keys_of, project_pad_only).unwrap();
            let mut streamed: Vec<Row<'static>> = Vec::new();
            let n = b
                .finish_each(
                    keys_of,
                    |src, buf| {
                        buf.extend(project_pad_only(src)?.values);
                        Ok(())
                    },
                    |cells| {
                        streamed.push(Row::new(cells.to_vec()));
                        Ok(())
                    },
                )
                .unwrap();

            assert_eq!(n, 2000);
            assert_eq!(collected, streamed, "budget {budget}");
        }
    }

    /// r855 — does the merge's per-row cost grow with the NUMBER of runs
    /// even now that picking a row is O(log k)?
    ///
    /// About 40 ms of the merge still has no owner. The obvious
    /// candidate — the `Vec<OrderKey>` allocated per row — is already
    /// weakened by round 845, which removed two allocations per row,
    /// 800k in all, and measured nothing.
    ///
    /// What has not been checked is locality. Round 852 timed decoding
    /// along one contiguous tape, which is the friendliest possible
    /// layout; the merge reads round-robin across k separate buffers,
    /// touching a different one almost every row.
    ///
    /// Budget fixed, row count varied, so k moves while the work done
    /// per row does not. A flat per-row cost means locality is not it.
    ///
    /// `cargo test -p spg-engine --release --lib r855 -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn r855_merge_cost_per_row_against_run_count() {
        extern crate std;
        use spg_storage::{ColumnSchema, DataType, Value};
        use std::time::Instant;

        let pad: alloc::string::String = "y".repeat(200);
        let cols = alloc::vec![
            ColumnSchema::new("id", DataType::Int, false),
            ColumnSchema::new("pad", DataType::Text, false),
        ];
        let descs = [false];

        for rows in [50_000usize, 100_000, 200_000, 400_000] {
            let mut s =
                ExternalSorter::new(Some(mem_run), 4 * 1024 * 1024, cols.clone(), &descs, &[]);
            for i in 0..rows {
                let r = Row::new(alloc::vec![
                    Value::Int(i32::try_from((i * 7919) % rows).unwrap()),
                    Value::text(pad.clone()),
                ]);
                let mut k = keys_vec(&r);
                s.push(&mut k, &r).unwrap();
            }
            let runs = s.runs.len() + usize::from(!s.ends.is_empty());
            let t = Instant::now();
            let out = s.finish(keys_of, |src| Ok(src.clone())).unwrap();
            let el = t.elapsed();
            std::eprintln!(
                "R855 rows={rows} runs={runs} merge={el:?} per_row={:.1}ns",
                el.as_nanos() as f64 / rows as f64
            );
            assert_eq!(out.len(), rows);
        }
    }

    /// r853 — is the merge's remaining time the linear head scan after all?
    ///
    /// Round 844 concluded it was not, from 29 runs merging no slower
    /// than 2. That comparison was confounded and the test said so at
    /// the time: the 2-run reading uses a 64 MB budget, so `finish`
    /// sorts a ~95k-row final batch in memory before merging anything,
    /// and that sort hid whatever the head scan was doing.
    ///
    /// With keys (~0 ms), the output clone (~13 ms) and decoding
    /// (~13 ms) all priced, the ~93 ms left has nowhere else to be:
    /// picking each output row scans every run's head, which at a 4 MB
    /// budget is 400k x 29 = 11.6M comparisons.
    ///
    /// Priced directly here. A loser tree would make it
    /// 400k x log2(29) ~ 2M.
    ///
    /// `cargo test -p spg-engine --release --lib r853 -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn r853_price_the_head_scan() {
        extern crate std;
        use std::time::Instant;

        const ROWS: usize = 400_000;
        const RUNS: usize = 29;
        let descs = [false];

        // One key per run head, as the merge holds them.
        let heads: Vec<Vec<OrderKey>> = (0..RUNS)
            .map(|i| alloc::vec![OrderKey::Int(i128::try_from(i * 7919).unwrap())])
            .collect();

        // What the merge does per output row: scan every head, keep the
        // smallest.
        let t = Instant::now();
        let mut sink = 0usize;
        for _ in 0..ROWS {
            let mut best = 0usize;
            for (i, k) in heads.iter().enumerate().skip(1) {
                if cmp_multi_key_in(k, &heads[best], &descs, &[]) == core::cmp::Ordering::Less {
                    best = i;
                }
            }
            sink += best;
        }
        let linear = t.elapsed();

        // What a loser tree would do: log2(RUNS) comparisons per row.
        let depth = usize::BITS as usize - RUNS.leading_zeros() as usize;
        let t = Instant::now();
        let mut sink2 = 0usize;
        for _ in 0..ROWS {
            let mut best = 0usize;
            for step in 0..depth {
                let i = (step + 1) % RUNS;
                if cmp_multi_key_in(&heads[i], &heads[best], &descs, &[])
                    == core::cmp::Ordering::Less
                {
                    best = i;
                }
            }
            sink2 += best;
        }
        let treeish = t.elapsed();

        std::eprintln!(
            "R853 linear({RUNS} heads)={linear:?} tree-depth({depth})={treeish:?} \
             cmps={} vs {} sink={sink}/{sink2}",
            ROWS * (RUNS - 1),
            ROWS * depth
        );
    }

    /// r852 — of the merge's 93 ms that is not keys and not the output
    /// clone, how much is DECODING?
    ///
    /// A spilled record comes back as bytes and has to become a `Row`
    /// again, which allocates and fills an owned `String` for the 200
    /// byte column, once per row. PG reads a tuple off its tape and
    /// passes it along without rebuilding it field by field, and spends
    /// about 70 ms on the whole of spilling where this merge phase
    /// alone spends 105 ms — so the rebuild is the candidate.
    ///
    /// Priced against the alternative that is not a design change:
    /// reading the same bytes back and not decoding them. The
    /// difference is what decoding costs; what is left is the read
    /// itself, which no design avoids.
    ///
    /// `cargo test -p spg-engine --release --lib r852 -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn r852_price_the_decode() {
        extern crate std;
        use spg_storage::{ColumnSchema, DataType, Value};
        use std::time::Instant;

        const ROWS: usize = 400_000;
        let pad: alloc::string::String = "y".repeat(200);
        let schema = TableSchema::new(
            "spg_sort_run",
            alloc::vec![
                ColumnSchema::new("id", DataType::Int, false),
                ColumnSchema::new("pad", DataType::Text, false),
            ],
        );

        // Lay the rows out exactly as a run does: length-prefixed bodies
        // in one buffer.
        let mut tape: Vec<u8> = Vec::new();
        for i in 0..ROWS {
            let row = Row::new(alloc::vec![
                Value::Int(i32::try_from(i).unwrap()),
                Value::text(pad.clone()),
            ]);
            let body = spg_storage::encode_row_body_dense(&row, &schema);
            tape.extend_from_slice(&u32::try_from(body.len()).unwrap().to_le_bytes());
            tape.extend_from_slice(&body);
        }
        std::eprintln!("R852 tape={} bytes for {ROWS} rows", tape.len());

        // (a) walk the tape, decode every row.
        let t = Instant::now();
        let mut at = 0usize;
        let mut decoded = 0usize;
        while at < tape.len() {
            let len =
                u32::from_le_bytes([tape[at], tape[at + 1], tape[at + 2], tape[at + 3]]) as usize;
            at += 4;
            let (row, _) = spg_storage::decode_row_body_dense(
                &tape[at..at + len],
                &schema,
                spg_storage::CURRENT_ROW_CODEC_VERSION,
            )
            .unwrap();
            core::hint::black_box(&row);
            at += len;
            decoded += 1;
        }
        let with_decode = t.elapsed();

        // (b) walk the same tape, touch the same bytes, decode nothing.
        let t = Instant::now();
        let mut at = 0usize;
        let mut seen = 0usize;
        while at < tape.len() {
            let len =
                u32::from_le_bytes([tape[at], tape[at + 1], tape[at + 2], tape[at + 3]]) as usize;
            at += 4;
            core::hint::black_box(&tape[at..at + len]);
            at += len;
            seen += 1;
        }
        let walk_only = t.elapsed();

        std::eprintln!(
            "R852 decode+walk={with_decode:?} walk_only={walk_only:?} rows={decoded}/{seen}"
        );
        assert_eq!(decoded, ROWS);
        assert_eq!(seen, ROWS);
    }

    /// r851 — how much of the merge is RE-DERIVING the sort keys?
    ///
    /// Every earlier reading of this used a `keys_of` that reads
    /// `values[0]` and is therefore free, so they all measured a merge
    /// that does not exist: the wiring hands `build_order_keys_bound`,
    /// which evaluates an expression per row. PG (measured through
    /// EXPLAIN, not read) spends about 70 ms on the whole of spilling
    /// where SPG's merge phase alone spends 103 ms, and the keys are
    /// the first candidate — PG writes them alongside the tuple and
    /// compares what it already has.
    ///
    /// Same rows, same merge, two `keys_of`: one free, one doing the
    /// work an expression evaluation actually costs.
    ///
    /// `cargo test -p spg-engine --release --lib r851 -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn r851_price_key_rederivation_in_the_merge() {
        extern crate std;
        use spg_storage::{ColumnSchema, DataType, Value};
        use std::time::Instant;

        const ROWS: usize = 400_000;
        let pad: alloc::string::String = "y".repeat(200);
        let cols = alloc::vec![
            ColumnSchema::new("id", DataType::Int, false),
            ColumnSchema::new("pad", DataType::Text, false),
        ];
        let descs = [false];

        // What every earlier bench used: a projected column, read back.
        fn free_keys(src: &Row<'static>, out: &mut Vec<OrderKey>) -> Result<(), EngineError> {
            match &src.values[0] {
                Value::Int(n) => {
                    out.push(OrderKey::Int(i128::from(*n)));
                    Ok(())
                }
                other => Err(EngineError::Internal(alloc::format!("bad key: {other:?}"))),
            }
        }

        // What the wiring hands it: a key that has to be COMPUTED. The
        // shape stands in for `build_order_keys_bound` walking a small
        // expression — an allocation and arithmetic per row, which is
        // what an ORDER BY operand costs when it is not a bare column.
        fn computed_keys(src: &Row<'static>, out: &mut Vec<OrderKey>) -> Result<(), EngineError> {
            let Value::Int(n) = &src.values[0] else {
                return Err(EngineError::Internal(alloc::string::String::from(
                    "bad key",
                )));
            };
            let Value::Text(t) = &src.values[1] else {
                return Err(EngineError::Internal(alloc::string::String::from(
                    "bad pad",
                )));
            };
            let folded = i128::from(*n) + i128::try_from(t.len()).unwrap_or(0);
            out.push(OrderKey::Int(folded));
            Ok(())
        }

        // Third variant: the same merge, but the projection keeps one
        // int instead of cloning the 200-byte row back out. If the
        // merge is bound by materialising rows rather than by any work
        // done per key, this is where it shows.
        for (label, keyfn, narrow) in [
            (
                "free",
                free_keys as fn(&Row<'static>, &mut Vec<OrderKey>) -> Result<(), EngineError>,
                false,
            ),
            ("computed", computed_keys, false),
            ("narrow-projection", free_keys, true),
        ] {
            let mut s =
                ExternalSorter::new(Some(mem_run), 4 * 1024 * 1024, cols.clone(), &descs, &[]);
            for i in 0..ROWS {
                let r = Row::new(alloc::vec![
                    Value::Int(i32::try_from((i * 7919) % ROWS).unwrap()),
                    Value::text(pad.clone()),
                ]);
                let mut k = Vec::new();
                keyfn(&r, &mut k).unwrap();
                s.push(&mut k, &r).unwrap();
            }
            let t = Instant::now();
            let out = if narrow {
                s.finish(keyfn, |src| {
                    Ok(Row::new(alloc::vec![src.values[0].clone()]))
                })
            } else {
                s.finish(keyfn, |src| Ok(src.clone()))
            }
            .unwrap();
            std::eprintln!("R851 {label} merge={:?} rows={}", t.elapsed(), out.len());
            assert_eq!(out.len(), ROWS);
        }
    }

    /// r844 — does the merge cost scale with the number of RUNS?
    ///
    /// Round 839 cleared the O(n·k) merge as the cause of a 1.33 s gap,
    /// and that was right. The gap is 72 ms now — SPG's spill machinery
    /// costs 224 ms against PG's 152 ms — and at that size a term
    /// dismissed as small deserves pricing rather than remembering.
    /// Picking each output row scans every run's head, so k runs cost
    /// n·k comparisons: 400k × 20 at a 4 MB budget.
    ///
    /// Same rows, three budgets, so k falls while n stays put. If the
    /// merge is flat in k, a loser tree buys nothing and the 72 ms is
    /// elsewhere.
    ///
    /// `cargo test -p spg-engine --release --lib r844 -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn r844_merge_cost_against_run_count() {
        extern crate std;
        use spg_storage::{ColumnSchema, DataType, Value};
        use std::time::Instant;

        const ROWS: usize = 400_000;
        let pad: alloc::string::String = "y".repeat(200);
        let cols = alloc::vec![
            ColumnSchema::new("id", DataType::Int, false),
            ColumnSchema::new("pad", DataType::Text, false),
        ];
        let descs = [false];

        for budget_mb in [4usize, 16, 64] {
            let mut s = ExternalSorter::new(
                Some(mem_run),
                budget_mb * 1024 * 1024,
                cols.clone(),
                &descs,
                &[],
            );
            for i in 0..ROWS {
                let r = Row::new(alloc::vec![
                    Value::Int(i32::try_from((i * 7919) % ROWS).unwrap()),
                    Value::text(pad.clone()),
                ]);
                let mut k = keys_vec(&r);
                s.push(&mut k, &r).unwrap();
            }
            let runs = s.runs.len() + usize::from(!s.ends.is_empty());
            let t = Instant::now();
            let out = s.finish(keys_of, |src| Ok(src.clone())).unwrap();
            std::eprintln!(
                "R844 budget={budget_mb}MB runs={runs} merge={:?} rows={}",
                t.elapsed(),
                out.len()
            );
            assert_eq!(out.len(), ROWS);
        }
    }

    /// r843 — the +0.15 s the wiring costs is not in the sorter, so it is
    /// in what the wiring does AROUND it. Three candidates, priced here
    /// against the same 400k rows the wall-clock runs use.
    ///
    /// The wired path does per row: clone the source row into the
    /// sorter, and later project it after the merge. The unwired path
    /// projects during the scan and never clones. So the delta should be
    /// one clone per row plus whatever moving the projection costs — this
    /// prices the clone, which is the part with no equivalent on the
    /// other side.
    ///
    /// `cargo test -p spg-engine --release --lib r843 -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn r843_price_the_wirings_extra_work() {
        extern crate std;
        use spg_storage::{ColumnSchema, DataType, Value};
        use std::time::Instant;

        const ROWS: usize = 400_000;
        let pad: alloc::string::String = "y".repeat(200);
        let rows: Vec<Row<'static>> = (0..ROWS)
            .map(|i| {
                Row::new(alloc::vec![
                    Value::Int(i32::try_from(i).unwrap()),
                    Value::text(pad.clone()),
                ])
            })
            .collect();

        // What the spill path adds: one full-row clone per row, on the
        // way into the sorter.
        let t0 = Instant::now();
        let mut sink: Vec<Row<'static>> = Vec::with_capacity(ROWS);
        for r in &rows {
            sink.push(r.clone());
        }
        let clone_cost = t0.elapsed();
        std::hint::black_box(&sink);

        // And it holds the output as a second Vec while the merge fills
        // it — priced separately because it is the same shape the
        // unspilled path already pays.
        let t1 = Instant::now();
        let out: Vec<Row<'static>> = sink.iter().map(Clone::clone).collect();
        let second_pass = t1.elapsed();
        std::hint::black_box(&out);

        let cols = alloc::vec![
            ColumnSchema::new("id", DataType::Int, false),
            ColumnSchema::new("pad", DataType::Text, false),
        ];
        std::hint::black_box(&cols);

        std::eprintln!(
            "R843 rows={ROWS} source_row_clone={clone_cost:?} extra_pass={second_pass:?}"
        );
    }

    /// r839 — where does a spilled sort spend its time? Counter-first,
    /// on the sorter alone: no scan, no wire. A MEMORY-backed run, so
    /// what this measures is codec plus merge, with disk I/O removed —
    /// if the cost is already here, the disk is not the problem.
    ///
    /// `cargo test -p spg-engine --release --lib r839 -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn r839_profile_spill_phases() {
        extern crate std;
        use spg_storage::{ColumnSchema, DataType, Value};
        use std::time::Instant;

        const ROWS: usize = 400_000;
        const WORK_MEM: usize = 4 * 1024 * 1024;

        let cols = alloc::vec![
            ColumnSchema::new("id", DataType::Int, false),
            ColumnSchema::new("pad", DataType::Text, false),
        ];
        let pad: alloc::string::String = "y".repeat(200);
        let descs = [false];

        // Build the inputs first, so generation is outside the timings.
        let t0 = Instant::now();
        let rows: Vec<Row<'static>> = (0..ROWS)
            .map(|i| {
                Row::new(alloc::vec![
                    Value::Int(i32::try_from((i * 7919) % ROWS).unwrap()),
                    Value::text(pad.clone()),
                ])
            })
            .collect();
        let build = t0.elapsed();

        let mut s = ExternalSorter::new(Some(mem_run), WORK_MEM, cols, &descs, &[]);
        let t1 = Instant::now();
        for r in rows {
            let mut k = keys_vec(&r);
            s.push(&mut k, &r).unwrap();
        }
        let push_phase = t1.elapsed();

        let t2 = Instant::now();
        let out = s.finish(keys_of, |src| Ok(src.clone())).unwrap();
        let merge_phase = t2.elapsed();

        std::eprintln!(
            "R839 rows={ROWS} build={build:?} push_spill={push_phase:?} merge_project={merge_phase:?} out={}",
            out.len()
        );
        assert_eq!(out.len(), ROWS);
    }

    /// The case the first two designs got wrong: the ORDER BY key is not
    /// in the projection. `SELECT pad FROM big ORDER BY id` is this
    /// shape. Spilling the SOURCE row makes it work by construction —
    /// the key is still there to be read.
    #[test]
    fn a_key_that_was_never_projected_still_orders_the_merge() {
        let descs = [false];
        let mut s = ExternalSorter::new(Some(mem_run), 1, record_cols(), &descs, &[]);
        for id in [7, 3, 9, 1, 8, 2, 6, 4, 5, 0] {
            let r = record(id);
            let mut k = keys_vec(&r);
            s.push(&mut k, &r).unwrap();
        }
        assert!(s.spilled(), "a 1-byte budget must have produced runs");
        let out = s.finish(keys_of, project_pad_only).unwrap();
        let pads: Vec<alloc::string::String> = out
            .iter()
            .map(|r| match &r.values[0] {
                Value::Text(t) => t.to_string(),
                other => panic!("pad column, got {other:?}"),
            })
            .collect();
        assert_eq!(
            pads,
            alloc::vec![
                "pad-0", "pad-1", "pad-2", "pad-3", "pad-4", "pad-5", "pad-6", "pad-7", "pad-8",
                "pad-9"
            ],
            "ordered by the unprojected key"
        );
        assert_eq!(
            out[0].values.len(),
            1,
            "the key column must not leak into the result"
        );
    }

    #[test]
    fn a_spilled_sort_returns_exactly_what_an_in_memory_sort_would() {
        let descs = [false];
        let mut s = ExternalSorter::new(Some(mem_run), 1, record_cols(), &descs, &[]);
        for id in [7, 3, 9, 1, 8, 2, 6, 4, 5, 0] {
            let r = record(id);
            let mut k = keys_vec(&r);
            s.push(&mut k, &r).unwrap();
        }
        assert!(s.spilled());
        let out = s.finish(keys_of, project_identity).unwrap();
        assert_eq!(ids_of(&out, 0), alloc::vec![0, 1, 2, 3, 4, 5, 6, 7, 8, 9]);
        assert_eq!(
            out[0].values[1],
            Value::text("pad-0".to_string()),
            "the whole row comes back, not only its sort key"
        );
    }

    #[test]
    fn descending_order_survives_the_merge() {
        let descs = [true];
        let mut s = ExternalSorter::new(Some(mem_run), 1, record_cols(), &descs, &[]);
        for id in [3, 1, 2] {
            let r = record(id);
            let mut k = keys_vec(&r);
            s.push(&mut k, &r).unwrap();
        }
        assert!(s.spilled());
        assert_eq!(
            ids_of(&s.finish(keys_of, project_identity).unwrap(), 0),
            alloc::vec![3, 2, 1],
            "DESC must survive the k-way merge"
        );
    }

    /// No factory: the old behaviour, byte for byte. A host with nowhere
    /// to spill must not start failing sorts it used to answer — and the
    /// projection still runs, so the result shape does not depend on
    /// whether a spill happened.
    #[test]
    fn without_a_factory_the_sort_stays_in_memory_and_still_sorts() {
        let descs = [false];
        let mut s = ExternalSorter::new(None, 1, record_cols(), &descs, &[]);
        for id in [2, 0, 1] {
            let r = record(id);
            let mut k = keys_vec(&r);
            s.push(&mut k, &r).unwrap();
        }
        assert!(!s.spilled(), "nothing to spill to means nothing spilled");
        let out = s.finish(keys_of, project_pad_only).unwrap();
        let pads: Vec<alloc::string::String> = out
            .iter()
            .map(|r| match &r.values[0] {
                Value::Text(t) => t.to_string(),
                other => panic!("pad column, got {other:?}"),
            })
            .collect();
        assert_eq!(pads, alloc::vec!["pad-0", "pad-1", "pad-2"]);
        assert_eq!(
            out[0].values.len(),
            1,
            "same output shape as the spilled path"
        );
    }

    /// r935 — the inline integer key has to put the batch in exactly the
    /// order the general comparator would, in the cases it takes AND in
    /// the ones it declines.
    ///
    /// The reference is the general comparator itself rather than a
    /// hand-written expectation: the fast path's only job is to agree
    /// with it, and a hand-written order would just be a second chance
    /// to make the same mistake.
    fn assert_order_matches(name: &str, keyrows: &[Vec<OrderKey>], descs: &[bool]) {
        let mut s = ExternalSorter::new(None, usize::MAX, record_cols(), descs, &[]);
        for (i, k) in keyrows.iter().enumerate() {
            let r = record(i as i32);
            s.push(&mut k.clone(), &r).unwrap();
        }
        let mut want: Vec<u32> = (0..keyrows.len() as u32).collect();
        want.sort_by(|&a, &b| {
            cmp_multi_key_in(&keyrows[a as usize], &keyrows[b as usize], descs, &[])
        });
        assert_eq!(s.sorted_order(), want, "{name}");
    }

    #[test]
    fn inline_int_key_orders_the_batch_like_the_general_comparator() {
        let int = |n: i128| alloc::vec![OrderKey::Int(n)];
        // Duplicates are in here on purpose: both sorts are stable, so
        // equal keys must come out in the order they were pushed.
        let plain: Vec<Vec<OrderKey>> = alloc::vec![
            int(5),
            int(-3),
            int(5),
            int(0),
            int(i64::MAX as i128),
            int(-1),
            int(5),
        ];
        let with_nulls: Vec<Vec<OrderKey>> = alloc::vec![
            int(7),
            alloc::vec![OrderKey::NullBig],
            int(-2),
            alloc::vec![OrderKey::NullSmall],
            int(7),
            alloc::vec![OrderKey::NullBig],
        ];
        // Declined: a real key sitting where a sentinel would.
        let at_the_ends: Vec<Vec<OrderKey>> =
            alloc::vec![int(3), int(i128::MAX), int(-4), int(i128::MIN), int(3)];
        // Declined: not an integer key at all.
        let texts: Vec<Vec<OrderKey>> = alloc::vec![
            alloc::vec![OrderKey::Text("pear".into())],
            alloc::vec![OrderKey::Text("apple".into())],
            alloc::vec![OrderKey::Num(1.5)],
        ];
        // v7.38.21 — taken now, by the first key, with the later keys
        // deciding inside a run. Every one of these has ties in the first
        // key: without them the run branch never executes and the case
        // proves nothing about it.
        let two_keys: Vec<Vec<OrderKey>> = alloc::vec![
            alloc::vec![OrderKey::Int(1), OrderKey::Int(9)],
            alloc::vec![OrderKey::Int(1), OrderKey::Int(2)],
            alloc::vec![OrderKey::Int(0), OrderKey::Int(5)],
            alloc::vec![OrderKey::Int(1), OrderKey::Int(2)],
            alloc::vec![OrderKey::Int(0), OrderKey::Int(5)],
        ];
        // A run whose first key is a NULL sentinel is still a run.
        let null_runs: Vec<Vec<OrderKey>> = alloc::vec![
            alloc::vec![OrderKey::NullBig, OrderKey::Int(3)],
            alloc::vec![OrderKey::Int(4), OrderKey::Int(1)],
            alloc::vec![OrderKey::NullBig, OrderKey::Int(1)],
            alloc::vec![OrderKey::NullSmall, OrderKey::Int(2)],
            alloc::vec![OrderKey::NullSmall, OrderKey::Int(0)],
        ];
        // The later key need not be an integer: inside a run it is the
        // general comparator that answers, exactly as before.
        let text_second: Vec<Vec<OrderKey>> = alloc::vec![
            alloc::vec![OrderKey::Int(2), OrderKey::Text("pear".into())],
            alloc::vec![OrderKey::Int(2), OrderKey::Text("apple".into())],
            alloc::vec![OrderKey::Int(1), OrderKey::Num(1.5)],
            alloc::vec![OrderKey::Int(2), OrderKey::Text("apple".into())],
        ];
        // Declined, and it must be: the FIRST key is what this path reads.
        let text_first: Vec<Vec<OrderKey>> = alloc::vec![
            alloc::vec![OrderKey::Text("b".into()), OrderKey::Int(1)],
            alloc::vec![OrderKey::Text("a".into()), OrderKey::Int(2)],
            alloc::vec![OrderKey::Text("b".into()), OrderKey::Int(0)],
        ];

        for (name, rows) in [
            ("plain ints", &plain),
            ("null sentinels", &with_nulls),
            ("keys at the ends of the range", &at_the_ends),
            ("non-integer keys", &texts),
        ] {
            assert_order_matches(name, rows, &[false]);
            assert_order_matches(name, rows, &[true]);
        }
        for (name, rows) in [
            ("two keys", &two_keys),
            ("two keys, null runs", &null_runs),
            ("two keys, text second", &text_second),
            ("two keys, text first", &text_first),
        ] {
            for descs in [[false, false], [false, true], [true, false], [true, true]] {
                assert_order_matches(name, rows, &descs);
            }
        }
    }

    /// v7.38.22 — a TEXT first key through the spilled sort, against the
    /// order the general comparator gives the same rows.
    ///
    /// Eight bytes decide most pairs and decide nothing for the rest, so
    /// the fixture is built to leave work for both: two hundred distinct
    /// strings whose first eight bytes are one of five prefixes, and a
    /// tail that orders them. A prefix that ordered a run wrongly, or a
    /// run left unsettled, both show up as a disagreement.
    #[test]
    fn a_text_first_key_survives_the_spill() {
        for descs in [[false], [true]] {
            let mut want: Vec<(alloc::string::String, i32)> = (0..200i32)
                .map(|i| {
                    (
                        alloc::format!("PFX{}xxxx-{:04}", i % 5, (i * 7919) % 200),
                        i,
                    )
                })
                .collect();
            let mut s = ExternalSorter::new(Some(mem_run), 4096, record_cols(), &descs, &[]);
            for (k, id) in &want {
                let r = record(*id);
                let mut keys = alloc::vec![OrderKey::Text(k.as_str().into())];
                s.push(&mut keys, &r).unwrap();
            }
            assert!(s.spilled(), "{descs:?}: the point is the spilled path");
            want.sort_by(|a, b| {
                let c = a.0.cmp(&b.0);
                if descs[0] { c.reverse() } else { c }.then(a.1.cmp(&b.1))
            });
            let out = s.finish(keys_of_text, project_identity).unwrap();
            assert_eq!(
                ids_of(&out, 0),
                want.iter().map(|w| w.1).collect::<Vec<i32>>(),
                "{descs:?}"
            );
        }
    }

    /// v7.38.21 — the two-key batch sort at a size that spills, against
    /// the order the general comparator gives the same rows.
    ///
    /// The three-row cases above all fit in one batch. A run that
    /// straddles a spill boundary is a different question, and this is
    /// the shape the endpoint sweep's `two keys` cell has: a first key
    /// with ties, 4,000 rows, a budget that cannot hold them.
    #[test]
    fn the_two_key_batch_sort_agrees_when_it_spills() {
        for descs in [[false, false], [true, false], [false, true]] {
            let mut want: Vec<(i128, i128, i32)> = (0..4000i32)
                .map(|i| (i as i128 % 3, ((i as i128) * 7919) % 13, i))
                .collect();
            let mut s = ExternalSorter::new(Some(mem_run), 4096, record_cols(), &descs, &[]);
            for &(a, b, id) in &want {
                let r = record(id);
                let mut k = alloc::vec![OrderKey::Int(a), OrderKey::Int(b)];
                s.push(&mut k, &r).unwrap();
            }
            assert!(s.spilled(), "{descs:?}: the point is the spilled path");
            want.sort_by(|x, y| {
                let first = if descs[0] {
                    y.0.cmp(&x.0)
                } else {
                    x.0.cmp(&y.0)
                };
                let second = if descs[1] {
                    y.1.cmp(&x.1)
                } else {
                    x.1.cmp(&y.1)
                };
                first.then(second).then(x.2.cmp(&y.2))
            });
            let out = s.finish(keys_of_two, project_identity).unwrap();
            assert_eq!(
                ids_of(&out, 0),
                want.iter().map(|w| w.2).collect::<Vec<i32>>(),
                "{descs:?}"
            );
        }
    }

    /// And the rows themselves come back in that order, spilled or not —
    /// `sorted_order` serves both the in-memory walk and the run writer,
    /// so a fast path that only the unspilled case exercised would leave
    /// the spilled one unpinned.
    #[test]
    fn inline_int_key_holds_when_the_sort_spills() {
        let descs = [false];
        for budget in [4096usize, 64 * 1024 * 1024] {
            let mut s = ExternalSorter::new(Some(mem_run), budget, record_cols(), &descs, &[]);
            for i in 0..2000i32 {
                let r = record((i * 7919) % 2000);
                s.push(&mut keys_vec(&r), &r).unwrap();
            }
            assert_eq!(s.spilled(), budget == 4096, "budget {budget}");
            let out = s.finish(keys_of, project_identity).unwrap();
            assert_eq!(
                ids_of(&out, 0),
                (0..2000i32).collect::<Vec<i32>>(),
                "budget {budget}"
            );
        }
    }
}
