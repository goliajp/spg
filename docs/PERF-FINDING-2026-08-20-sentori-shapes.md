# Finding — SPG loses 1.6×–8.9× on sentori's own shapes

**Date:** 2026-08-20 · **Against:** `goliakk/spg:7.38.7` vs
`postgres:18-alpine`, same box, interleaved, N=6, each sample min-of-3
· **Harness:** `xbench/dropin-perf/run.sh`, profile
`profiles/sentori`

Sentori said they would measure us against the image their compose
ships today, on their shapes, and that their gate stays at zero until
they have. We built the harness they asked for and ran it first. We
lose, and not by a little.

## The measurement

mini, load 6.4 → 9.5 across the run. The control leg — the reference
image compared against itself — was clean on 7 of 8 cells, so the one
cell it flagged is the only one whose magnitude is in question. The
same profile on a second box (load 7.9, control dirty on 4 of 8) put
the worst cell at 675% where mini put it at 786%: two machines, two
noise floors, same band.

| shape | SPG 7.38.7 | PG 18 | verdict |
|---|---|---|---|
| ingest: one row | 6.6-44.8 | 2.0-108.2 | unresolved |
| window: count over a day | 24.8-27.0 | 8.0-15.4 | **SLOWER 60%** |
| window: group by kind | 26.3-29.2 | 7.1-8.1 | **SLOWER 225%** |
| window: distinct seats | 40.7-45.4 | 14.0-21.2 | **SLOWER 92%** |
| jsonb: containment | 20.6-22.9 | 8.7-9.9 | **SLOWER 108%** |
| jsonb: containment in a window | 65.0-69.3 | 6.9-7.3 | **SLOWER 786%** |
| btree: project and kind | 0.19-0.24 | 0.21-0.30 | unresolved |
| dashboard: top versions | 35.2-36.2 | 5.1-6.6 | **SLOWER 436%** (control dirty) |

## Two named causes, from the plans

Not "we are slower". `EXPLAIN` on both sides for the worst cell:

```
SPG    Aggregate
         -> Seq Scan on events
              Filter: (traits @> …) AND (received_at >= …) AND (received_at < …)

PG18   Finalize Aggregate -> Gather (Workers Planned: 2)
         -> Parallel Bitmap Heap Scan on events
              Recheck Cond: (received_at >= … AND received_at < …)
              Filter: (traits @> …)
              -> Bitmap Index Scan on events_time
```

**1. A range predicate on an indexed timestamp does not drive an index
scan.** `events_time` is a BRIN index; PG plans a bitmap index scan
over it and touches a fraction of the table, and we read all 200,000
rows. Every window shape in the table above is this one cause. The
plain one-day count is the cleanest statement of it: same predicate,
no jsonb, still sequential.

**2. There is no intra-query parallelism.** PG18 gathers two workers on
these scans. The `jsonb: containment` cell is the control for this:
PG18 *also* chooses a sequential scan for that predicate, and still
wins 2.08× — which is what two workers buy and nothing else. So a ~2×
floor sits under every large scan we run, independent of the planner.

A third observation, not yet a cause: our cost estimate reports
`rows=66666` for every one of these predicates — a fixed one-third
guess. Whatever selectivity the statistics hold is not reaching the
plan.

## What this is not

It is not the harness. The control leg was clean on the cells it
flags, the two boxes agree on the band, and the `btree` cell — the one
shape that has an ordinary index and a small answer — is a tie, which
is the shape of a real result rather than a systematic bias.

It is not the jsonb encoding either: `jsonb: containment` and
`btree: project and kind` bracket it. One is 2.08× on a predicate PG
also scans sequentially, the other is a tie.

## Next

Phase A decomposition against PG18's scan path, per
`perf-decomposition-vs-polish` — the two causes above are entry
points, not a diagnosis. The Pre-Phase-A gate is satisfied: the gap is
measured against the reference the customer actually runs, on a box
whose noise floor was measured in the same run, and it reproduces on a
second machine.
