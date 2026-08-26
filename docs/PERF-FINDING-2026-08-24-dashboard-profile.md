# The dashboard shape, under a profiler

**Status: the named attack is DONE, 2026-08-24. Two of the shapes it
touches now beat PostgreSQL; the dashboard is 1.42x from 1.87x. The two
reverted attacks stay recorded below, because two of the three readings
in this document were wrong and the record of that is worth more than a
tidy page.**

| shape | before | after | PG 18 |
|---|---:|---:|---:|
| `count(*) WHERE project_id = 3` | 0.940 ms | **0.408** | 0.789 |
| `SELECT id WHERE project_id = 3` | 0.951 | **0.410** | 0.791 |
| `dashboard: top versions` | 6.473 | **5.436** | 3.828 |

Sentori's `dashboard: top versions`:

```sql no-run
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

## The attack, done

`Seeked { rows, exact }`. `exact` is FALSE everywhere it is not proven —
a miss costs a re-check, a false claim silently drops or invents rows.
Each arm earned the claim separately:

  * single-column equality on a non-collated key — exact
  * leading-prefix walk, when the predicate IS that one equality — exact
  * range walk on a non-collated key — exact
  * a collated key — NOT exact, the probe is an encoding
  * GIN / trigram / jsonb containment — NOT exact, by construction
  * anything with a residual conjunct — NOT exact

"Stands for the value" is asked of the KEY rather than the value: a key
that exists came through `probe_key`, which already refused a collated
column and one whose comparison folds, so what is left is the type and
the key's variant is the honest way to ask it. `DATE` and `TIMESTAMP`
are absent from the allowlist even though both index as integers —
they index as the SAME integer variant, so a key cannot say which it
came from.

Nine answer tests, every count taken from PostgreSQL 18.4 rather than
reasoned about — the first draft of the prefix test asserted 30 where
the answer is 15 and failed on the code being right. The negative
control flips every arm to claim exactness; three tests go red. Two do
NOT bite and say so in their own text: the collated equality is exact in
FACT (the sort key appends the original bytes, so two strings never
share a key) and is still not claimed exact, and no jsonb shape could be
found where the candidate set is wider than the answer — six were
tried.
