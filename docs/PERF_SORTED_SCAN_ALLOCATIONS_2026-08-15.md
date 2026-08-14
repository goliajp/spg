# The row-returning sorted scan allocates twice per row

Phase A, then Phase B underneath it.

**Read the r1031 section at the bottom first.** Phase A measured the
EMBEDDED executor throughout and called it "the row-returning sorted scan".
The server runs a different one — the external sorter — and that one
allocates four times per row, not twice. Everything above the divider is
correct about the path it measured and wrong about which path the server
takes.

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

---

## r1031 — what got built, and the thing the Phase A above got wrong

Attack 1 is implemented as `try_int_key_sorted_stream`
(`crates/spg-engine/src/select.rs`): ORDER BY over up to four integer
columns, keys carried in a fixed array with a NULL bitmask instead of a
`Vec<OrderKey>` per row. Interleaved, three rounds, two binaries built
minutes apart:

| `SELECT k FROM t ORDER BY k`, 400 k | base | lane |
|---|---:|---:|
| allocations per query | 800,068 | **400,054** |
| time | 62.22-68.04 ms | **43.05-46.60 ms** |

Exactly halved, −31 %, non-overlapping.

**And it is unreachable from the server.** The same two binaries, same
probe, with a spill sink installed — which is the ONLY difference between
`Engine::new()` and what `spg-server` configures:

| same query, spill sink present | base | lane |
|---|---:|---:|
| allocations per query | 1,600,236 | 1,600,236 |
| time | 56.63-64.27 ms | 57.08-63.42 ms |

Identical. `try_spill_sorted_stream` runs first and takes the query, so
the lane never sees it. Two endpoint A/B runs had already reported
`B_faster=0 B_slower=0` with a clean control leg; that was a correct
measurement of a change the server cannot reach, and it took installing
the sink in the probe to say so rather than guess.

So the lane is an embedded-path (SPGE) improvement, and the document above
measured the embedded path while calling it "the row-returning sorted
scan". The server's row-returning sorted scan is the external sorter, and
it allocates **four times per row**, not twice.

### The next target, measured

`sample` over the probe in the server configuration, 400 k rows:

| leaf | samples |
|---|---:|
| allocator family (`xzm_free`, `xzone_malloc`, `free`, …) | **2,564** |
| `ExternalSorter::sorted_orders` quicksort | 992 |
| `platform_memmove` | 920 |
| `orderby::build_order_keys_bound` | 525 |
| `extsort::HeadHeap::sift_down` | 466 |
| `ExternalSorter::push` | 402 |
| `orderby::order_key_elem_cmp` | 363 |
| `codec::decode_row_body_dense_pruned` | 293 |

Four per row is one `Vec<OrderKey>`, one row buffer on the way in, and the
serialise/deserialise pair across the run file. Attacking it is a change to
`extsort.rs`, not to the scan — and it is on shapes SPGS currently WINS
(`narrow` 73.4-75.6 against PG18's 104.1-126.7), so it is a lead to widen,
not a loss to close.

---

## r1032 — the merge stopped allocating to read four bytes

The call tree named all four of the external sorter's per-row allocations:

| # | site | what |
|---|---|---|
| 1 | `extsort.rs:302` `read_exact(run, 4)` | **a fresh `Vec<u8>` to hold a four-byte length prefix** |
| 2 | same function, the body read | another `Vec<u8>` |
| 3 | `codec.rs:1935` | the decoded row's `Vec<Value>` — the answer itself |
| 4 | `orderby.rs:1618` | the row's `Vec<OrderKey>` |

1,600,236 = 4 × 400,000 + 236, so the count agrees with the tree.

The first two are pure ceremony: `read_exact` allocated its own buffer on
every call, and the merge calls it twice per row. It now fills a buffer the
merge owns, which grows once to the widest row and then stops.

Interleaved, five rounds, two binaries, server configuration:

| | base | reused buffer |
|---|---:|---:|
| allocations per query | 1,600,236 | **800,231** |
| time | 57.30-63.45 ms | **50.65-53.36 ms** |

Halved, −11 %, and no round overlaps. The embedded configuration is the
negative control in the same run: identical allocation counts on both legs,
because the external sorter is not on that path.

What is left is two per row, and both are real: the decoded row is the
answer, and the `Vec<OrderKey>` is the tournament's comparison key. The
keys are the more attackable of the two — only `runs.len()` heads are alive
at once, so their buffers could be recycled rather than reallocated, which
would need `keys_of` to write into a caller's buffer instead of returning
one.

### Does it reach the server, and can the wire see it?

Two different questions, and the r1031 lane made the mistake of answering
the first by looking at the second.

**Reach — yes, witnessed.** `pg_stat_database.temp_files` against a running
server, on the sweep's own table and its own `work_mem = 4096`:

| | temp_files |
|---|---|
| `SELECT id FROM sw ORDER BY k`, work_mem 4 MB | 0 → **6** |
| the same query, work_mem 4 GB | 6 → 6 |

It spills at the setting the endpoint panel uses, and does not when given
room — the counter moving in one direction and holding still in the other
is what makes it a witness rather than a reading.

**Wire visibility — no, and the harness says why.** Three legs at N=31 on
`narrow, non-indexed key`: A 71.9-80.6, B 72.8-80.9, C 72.8-81.5. The
control leg is the SAME BINARY as A and spans nine milliseconds; the effect
is about seven. Raising N did not narrow it, because that spread is machine
drift rather than sampling noise.

So: −11 % in process, on the path the server takes, below what this testbed
can resolve at the socket. Not a wire win, and not evidence of absence
either.
