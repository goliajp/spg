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

## Ablation — where the time actually goes

The plans above name what we do differently. They do not price it, so
the same profile was taken apart by ablation instead: the same query
with one clause removed at a time, min-of-3 in one session, both
engines in the same window. All figures ms over 200,000 rows.

| probe | SPG | PG 18 | SPG cost per row |
|---|---|---|---|
| bare `count(*)` | 2.9 | 4.8 | — (**we win**) |
| `received_at IS NOT NULL` (touch, no compare) | 5.5 | 4.5 | 13 ns |
| `id > id` (bigint, column vs column) | 3.3 | 5.3 | ~2 ns |
| `id > 0` (bigint vs literal) | 7.4 | 5.2 | 22 ns |
| `kind > kind` (text, column vs column) | 10.9 | 4.6 | 40 ns |
| `kind = 'click'` (text vs literal) | 6.8 | 3.5 | 20 ns |
| `received_at > received_at` (column vs column) | 7.0 | 4.7 | 20 ns |
| **`received_at > timestamp '…'`** | 13.5 | 5.4 | **52 ns** |
| `traits IS NOT NULL` (touch jsonb) | 5.7 | 4.6 | 14 ns |
| **`traits->>'plan' = 'pro'`** | 83.2 | 6.7 | **398 ns** |
| `traits->>'seat' = '42'` (LAST field) | 80.0 | 6.8 | 398 ns |

Two things this settles.

**Scanning is not the problem — we beat PG on the bare count.** What
we lose is entirely per-row predicate work, so an attack on the scan
loop itself would be aimed at the one part of this that already wins.

**The jsonb cost does not depend on which field is asked for.** First
field and last field are the same price, which is the shape of parsing
the whole document rather than walking to a position. And the
representation says why: `Value::Json(Cow<str>)` — jsonb is a string
at rest, in memory and on disk, so every field access re-parses. PG
stores a binary tree with an offset table and answers in 11 ns.

## One prediction, made and refuted

Worth recording because the refutation is the useful part.

`constfold` (r1042) folds `timestamp '2026-06-01'` at prepare time, so
the cast is gone from the tree. But its exit converts the folded
`Value` back into a `Literal`, and `clock.rs:35` reads

```rust
Value::Timestamp(t) => Literal::String(eval::format_timestamp(t)),
```

— the folded constant becomes a *string*. `Date`, `Uuid`, `Bytes` and
`Numeric` take the same exit. So the predicate that runs is a
timestamp column compared against a string, and the obvious reading is
that each row clones that string and parses it back into a timestamp.

The discriminator for that reading is `id > '0'`: an integer column
against a string literal must parse per row too, if the mechanism is
real. It costs 7.72 ms against 7.58 ms for `id > 0` — 0.7 ns a row
apart. **The mechanism as stated is not what is happening**, at least
not for integers, and a story that only holds for one type is not yet
a diagnosis.

What is certain is the code: the fold's exit cannot express a typed
constant, so typed constants leave it as text. What is not established
is how that turns into the 31 ns a timestamp comparison costs over an
integer one. The next step is a leaf-symbol profile of exactly that
query — the category first, per the methodology, since the counters
and the ablation have now given prices for a mechanism nobody has
seen.

## Three predictions, all refuted, and the diagnosis they cornered

The useful part of this section is that every mechanism proposed from
reading the code was wrong, and each refutation narrowed the target.

**Refuted 1 — "the folded constant is cloned and re-parsed per row."**
`constfold` folds `timestamp '…'` and `clock.rs:35` turns the folded
`Value::Timestamp` back into a `Literal::String`. Discriminator: an
integer column against a *string* literal would pay the same clone and
parse. `id > '0'` costs 7.72 ms against `id > 0`'s 7.58 — 0.7 ns a row
apart. Not it.

**Refuted 2 — "the typed spelling is what does it."** If the fold's
exit were the cause, an unadorned literal (unknown-typed, resolved
against the column at prepare time) would be cheaper. All three
spellings — `timestamp '1900-01-01'`, `'1900-01-01'`, and
`'1900-01-01'::timestamp` — cost 14.6, 15.9 and 14.3 ms. Not it.

**Refuted 3 — "the compiled fast lane declines and we fall back per
row."** There is a `PredShape::ColumnCmpLit` lane that compares in
place. Counter, over 200,000 rows: it **fires 200,000 times** on the
timestamp shape, exactly as it does on the integer shape. The
timestamp predicate is five times the price of the integer one
*inside the same lane*. Not a lane miss.

(The jsonb shape is different in kind: the fast lane fires **zero**
times there. Its 400-765 ns a row is a different path and a different
campaign.)

**The diagnosis.** `binop.rs:5742` — `compare`'s same-variant fast
match — covers `Int/Int`, `BigInt/BigInt`, `SmallInt/SmallInt`,
`Text/Text` and `Bool/Bool`. It does not cover `Timestamp`, `Date`,
`Time`, `Uuid`, `Bytes` or `Numeric`. Its own comment says why the
list is what it is: r483 measured the pre-check chain below it at
35.6 % of self time on `g = 5`, with `Value::data_type` another 4.5 %,
and added the arms for the variants that were hot *then*.

A timestamp comparison misses that match twice over. The column is
`Timestamp`; the literal reaches the row loop as `Text`, whatever the
spelling, because `Literal` has no temporal variant to hold it. So
every row walks the full guard chain and then coerces the text into a
timestamp. `Value::data_type` showing up at 0.71 % of self time in a
sample of exactly this query is that chain.

## Top-N attacks

| # | file:line | change | est | semantic | blast |
|---|---|---|---|---|---|
| 1 | `eval/binop.rs:5742` | add same-variant arms to `compare`'s fast match: `Timestamp`, `Date`, `Time`, `Uuid`, `Bytes`, `Bool` pairs already there | removes the guard walk when both sides are the same variant | none — identical answer by the same arms | one match |
| 2 | `spg-sql/src/ast.rs` (`Literal`) + `clock.rs:35` | `Literal::Timestamp(i64)` / `Literal::Date(i32)` so a folded or resolved temporal constant reaches the row loop typed rather than as text | removes the per-row coercion; with #1 it lands in the fast match | none if the text form stays the display form | AST + fold exit + literal→value |
| 3 | jsonb representation | binary form with an offset table instead of `Value::Json(Cow<str>)` | the 400-765 ns a row | none observable | large — own campaign |

`Literal::Integer` is already `i64`, so #2 adds no width to the enum —
the tax a wider variant would put on every existing literal (v7.37.26)
does not apply.

#1 and #2 are one change in effect: #1 without #2 helps only
column-vs-column comparisons, and #2 without #1 skips the coercion but
still walks the chain.

## Next

Phase A decomposition against PG18's scan path, per
`perf-decomposition-vs-polish` — the two causes above are entry
points, not a diagnosis. The Pre-Phase-A gate is satisfied: the gap is
measured against the reference the customer actually runs, on a box
whose noise floor was measured in the same run, and it reproduces on a
second machine.
