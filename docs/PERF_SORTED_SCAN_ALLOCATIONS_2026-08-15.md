# The row-returning sorted scan allocates twice per row

Phase A. Measured and attributed; nothing implemented. The attack list at
the end is what Phase B should start from.

## How this came up

Closing the `distinct then order` cell (r1030) needed a counting global
allocator, and the control leg it was measured against reported a number
worth its own investigation:

| 400 k rows, in process | allocations per query | bytes |
|---|---:|---:|
| `SELECT k FROM t ORDER BY k` | **800,067** | 208.7 MB |

Two allocations per row, and 208 MB of allocation traffic, for a query
whose answer is four hundred thousand integers.

## Which two

Sampling by allocation COUNT (one backtrace every 4096 allocations —
by bytes would barely see allocations this small), summed over four
executions, `crates/spg-engine/examples/probe_distinct_unique.rs`:

| owner | allocations |
|---|---:|
| `reserve<spg_engine::orderby::OrderKey>` | 1,654 k |
| `reserve<spg_storage::Value>` | 1,650 k |

Two per row, split evenly: one `Vec<OrderKey>` holding the row's sort keys,
one `Vec<Value>` holding its projected columns. `tagged: Vec<(Vec<OrderKey>,
Row)>` is that pair, one heap allocation each, four hundred thousand times.

The pools added in rounds 485 and 571 (`proj_pool`, `key_pool`) recycle
these — but only along the top-N trim, which drops rows. A full scan keeps
every row, so every row keeps its buffers and the next row allocates fresh.

## What makes the key half worse than it looks

`sort_tagged_by_inline_int_key` (`orderby.rs:1258`) already sorts indices
rather than rows: for a single integer key it builds `Vec<(i128, u32)>`,
sorts that, and permutes the rows once. The profile confirms it is the path
these shapes take — its quicksort is 545 of the distinct leg's samples and
886 of the plain leg's.

So on this shape the per-row `Vec<OrderKey>` is built, has one `i128`
extracted from it, and is then dragged through the permutation. It exists
only to carry an integer that the row's column already held.

That is the same shape as the predicate VM before its integer lane
(`PredShape::IntArith`, 69.1 → 11.7 ns/row): a general representation
materialised per row for a case that never needed it.

## Attack list

1. **A typed key lane for the sort.** When every ORDER BY term binds to a
   column of an integer type, carry `(i128, Row)` from the scan instead of
   `(Vec<OrderKey>, Row)`. Removes the per-row key allocation, the
   `OrderKey` construction, the later extraction pass, and the
   `Vec<Option<..>>` permute. Decided at plan time from the bound column's
   type, not after collection the way `sort_tagged_by_inline_int_key`
   decides now.
   Sites: `select.rs:737, 3244, 5939, 6659, 8551` (the five `tagged`
   declarations), `orderby.rs:1213 partial_sort_tagged_in`,
   `orderby.rs:1381 topk_trim`, `orderby.rs:1467 cmp_multi_key_in`.

2. **A stride-flattened key arena for the general case.** ORDER BY width is
   fixed per query, so `n_rows × stride` in one buffer, with row `i`'s keys
   at `[i*stride..]`, removes the allocation for non-integer keys too. This
   is the shape that worked on the sort-spill path in r851-r884 (234 → 180
   ms, 253 MiB → 12 MiB); the difference is that this buffer must survive
   `topk_trim` dropping rows from the middle.

3. **The projected row.** The other half, and the harder one: `Row` owns its
   `Vec<Value>` and travels to the encoder. Worth attacking only after (1)
   and (2) have been measured, so its own contribution is visible rather
   than inferred.

## Why now, on shapes that already win

`two keys` at 400 k reads SPGS 178.6-193.1 against PG18 166.4-249.4 — the
ranges overlap so the harness says unresolved, but PG's floor is below ours.
That is the same signature `distinct then order` carried before it was
attacked, and it was a genuine 25 % once measured.

## What this does NOT say

How much of the 800 k allocations is recoverable in time. The DISTINCT
change priced ITS allocations at 46 ns apiece after the fact, having
predicted 70 — half again too high. Applying either figure here would be
predicting, not measuring. Phase B measures before and after on this same
probe.
