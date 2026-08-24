# The dashboard shape, under a profiler

**Status: profiled. Two attacks tried and REVERTED, one named and not
started. The remaining gap has a cause and a design.**

Sentori's `dashboard: top versions`:

```sql
SELECT traits->>'version' AS v, count(*) FROM events
WHERE project_id = 3 GROUP BY 1 ORDER BY 2 DESC, 1 LIMIT 10
```

200,000 rows, 25,000 matching. **SPG 7.1 ms against PostgreSQL 18's
3.8** — 1.87x, down from 2.45x earlier in v7.38.19 and from 15.3 ms
before it.

## The profile

`sample` on the server while the query ran in a loop; leaf symbols,
waiting frames removed, aggregated by CATEGORY — under fat-LTO a symbol
absorbs what was inlined into it, so the line is not to be trusted and
the category is.

| | samples |
|---|---:|
| binary-operator dispatch and work | 2,811 |
| allocator | 1,251 |
| jsonb scanning | 987 |
| memmove / memcmp | 762 |
| aggregate | 328 |
| index seek | 312 |

## Two attacks, both refuted by measurement

**1. Route the JSON accessors before the dispatch chain.**
`apply_binary` is a linear chain of nineteen gates and the `->` `->>`
arms are at the end of it; its own code carried 1,332 samples against
987 for all of the jsonb work it dispatches to, so getting to the
operator looked like it cost more than the operator.

Twelve interleaved rounds against a binary built without it: **6.6–9.0
against 6.6–7.3 — neutral to slightly worse.** Reverted.

What that refutes is the reading, not the measurement: under fat-LTO
`apply_binary` had the operator bodies inlined into it, so its 1,332
samples were never the walk.

**2. Stop allocating the group tuple and its key per row.**
`accumulate_groups` built a fresh `Vec` for the group values and a
fresh `String` for the key it hashes, for every row —
`encode_key_refs_into` has existed since v7.31 for exactly this reason
("tens of thousands of String allocations per query just to look up a
map") and this call site never got it.

Twelve interleaved rounds: median **7.123 → 7.072**, after ahead in 9
of 12. **0.7 %.** Reverted: strictly less work, but not work that was
being paid for, and an unmeasurable change is one more thing that can
be wrong.

## What the profile then said, once asked properly

Re-decomposed against PostgreSQL, every stage warmed first — the first
cold cell had reported 3.383 ms for a query the next row does strictly
more work than, which is a cache and not a measurement:

| stage (25,000 matching rows) | SPG | PG 18 | what the step adds to the gap |
|---|---:|---:|---|
| `count(*)`, predicate only | 3.193 | 0.804 | |
| + group by a plain text column | 1.715 | 1.013 | −67 ns/row |
| + group by `traits->>'version'` | 6.857 | 3.703 | **+98 ns/row** |
| + `ORDER BY 2 DESC, 1 LIMIT 10` | 7.067 | 3.693 | +9 ns/row |

Read the first two rows against each other. **`count(*)` with a
predicate costs more than the same query with a GROUP BY bolted on** —
3.2 against 1.7, reproducibly, across four interleaved rounds. More work
for less time.

## The cause

Profiling the two shapes separately, leaves, waits removed:

```text
  count(*) WHERE project_id = 3          count(*) … GROUP BY project_id
    try_index_seek           1814          accumulate_groups        2459
    binop::compare           1633          try_index_seek           1192
    run_single_table_agg…     309          binop::compare           1063
    try_leading_composite…    253          eval_compiled_pred        195
```

Both are the index walk and a comparison. The comparison is
`project_id = 3`, re-evaluated once per candidate — and `compare`'s very
first arm is `(Int, Int) => a.cmp(b)`, so it is not that each comparison
is expensive. It is that **the comparison should not be happening.**

The composite prefix walk returns exactly the rows whose leading key
component equals the probe. For an integer key that is not an
approximation — key equality IS value equality. The caller re-applies
the whole `WHERE` to each candidate because the seek's contract says the
answer is a CANDIDATE set, which is true of the GIN, trigram and jsonb
seeks and of a collated key, and not true here.

## The attack, not started

Let the seek report whether its answer was EXACT for the whole
predicate, and let the caller skip the re-check when it was.

Not started because it is a contract change across the whole seek
family, and an arm that claims "exact" when it is over-approximate
returns silently wrong rows — the failure mode this codebase has been
bitten by twice this version alone. Each arm has to earn the claim
separately:

  * single-column equality on a non-collated key — exact
  * leading-prefix walk, when the predicate IS that one equality — exact
  * range walk on a non-collated key — exact
  * a collated key — NOT exact, the probe is an encoding
  * GIN / trigram / jsonb containment — NOT exact, by construction
  * anything with a residual conjunct — NOT exact

It wants its own round, with a pin per arm and a negative control that
puts a row in the candidate set the predicate rejects.
