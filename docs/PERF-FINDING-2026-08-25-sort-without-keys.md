# The sort key was a copy of a column the output already held

*2026-08-25 — v7.38.19*

## What was measured

The release sweep's `sort only, long text distinct` cell:

```sql no-run
SELECT count(*) FROM (SELECT s_long FROM s ORDER BY s_long) z
```

400,000 rows, `s_long` ~192 bytes with no two rows sharing a prefix,
both engines ordering BYTES (`SPG_LC_COLLATE=C` against a PG18 `C`
database). This cell was the only red on the gate, at 3.70x — over the
3.0x ceiling.

Every number below is a median of n=18: six rounds alternating the two
binaries, three timings each, **one server at a time**. That last part
is not decoration; see "How the first measurement lied".

## Where the time was

A leaf profile put 2,025 samples in the allocator's free path and 120
in malloc. A `count(*)` over a sorted subquery allocates nothing of its
own, so that is the sort key: one `OrderKey` per row holding a COPY of
the column — 400,000 allocations, 400,000 frees, 77 MB moved.

The copy exists for a reason. By the time the sort runs, the source row
is gone: the scan projected what the query asked for and dropped the
rest. A key that borrowed from the row would dangle.

But when the ORDER BY names a column the projection *already carries*,
the row is not gone. The value is sitting in the output.

## The two wins, in the order they appeared

    baseline                            382.5 ms
    no key built                        344.6 ms   0.90x
    + the Text/Text compare inlined     303.0 ms   0.79x

**The second one is larger, and it was not predicted.** With the keys
gone, the profile's top working symbol was `orderby::value_cmp` at 697
samples against 90 in `memcmp`. The string comparison was never the
cost. The CALL was.

`value_cmp` carries the NUMERIC bignum gate, the float total order and
the MySQL fold. It is too big to inline into a loop that runs it
~7 million times, so every one of those comparisons paid a call and two
discriminant loads to reach an arm that is `x.cmp(y)`. Answering the
one pair a text sort actually asks — two non-NULL strings, no fold —
inside the loop gives the same answer by the same route.

This is the same shape as the seek-exactness round three days earlier,
where `binop::compare`'s first arm was `(Int, Int) => a.cmp(b)` and the
cost was that 25,000 comparisons were re-deciding what the index walk
had already decided. Twice now the expensive thing has been *reaching*
a cheap comparison.

## The guard is the whole risk

A sort KEY encodes what a column MEANS. A value is what it holds. For
several types those are different orders:

| type | value order | key order |
|---|---|---|
| user ENUM | its label, as text | declaration position |
| array | rendered form | element-wise |
| collated text | bytes | the collator |

The first draft had no guard, and eleven tests across four files went
red — including one that answered `["high", "mid"]` where the answer is
`["mid", "high"]`. That is a **silently wrong result**, not an error.

`value_order_is_key_order` is the short list where the two agree:
non-enum, non-domain, non-composite, uncollated, and one of
`smallint / int / bigint / text / varchar / bool / uuid`.
`crates/spg-engine/tests/e2e/e2e_sort_without_keys.rs` holds it from
the other side: remove any clause and one of those tests returns a
wrongly ordered result.

The path is restricted further, to the full uncollated sort. A top-N
compares against a stored boundary key and `WITH TIES` extends past the
limit through the keys — both need keys to exist. A collation would
have to be resolved per comparison, which is the per-row cost v7.38.18
removed from the scan filter.

## How the first measurement lied

Twice, and both failures looked exactly like data.

**The A/B compared a binary against itself.** The two servers were
launched as `/tmp/spg-old` and `/tmp/spg-new`, so their process names
are `spg-old` and `spg-new` — and the harness's teardown was
`pkill -f spg-server`, which matches neither. Every run reached the
first server still holding the port. Thirty-six timings, perfectly
neutral, and the only tell was the fixture growing 400k → 800k → 1.2M
→ 1.6M as each "fresh load" appended to the same live instance.

**Then the branch was never entered.** The A/B database was created at
the ssh session's `en_US.UTF-8`, and the fast path refuses a collated
column by design. The cell it was meant to measure runs at `C`. The
ablation that caught it: make the branch do `tagged.truncate(1)` and
watch the count. 400000 means the branch was not taken; 1 means it was.

Both of these are the standing rule in a new costume — *the failure of
a measuring device looks exactly like data*. What broke each one was a
witness unrelated to the quantity: a row count that should not change,
and a count that must change. The harness now asserts both before it
times anything, plus that exactly one process matching the requested
binary path is serving.

## Files

- `crates/spg-engine/src/select.rs` — `order_by_output_cols_if_identical`,
  `value_order_is_key_order`, and the no-key branch in
  `run_single_table_scan`
- `crates/spg-engine/tests/e2e/e2e_sort_without_keys.rs` — the pins
