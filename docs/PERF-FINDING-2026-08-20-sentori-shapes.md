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

## Where the campaign stands, and the finding that outranks the rest

Three changes landed (v7.38.8): temporal constants carried decoded,
the json column boundary enforced so the accessors stop re-validating,
and object keys compared in place. Same box, interleaved, min of four,
against a v7.38.7 server and postgres:18 on the same machine:

| shape | 7.38.7 | now | PG 18 |
|---|---|---|---|
| window: count over a day | 21.4 | 5.0 | 1.2 |
| window: group by kind | 22.6 | 5.3 | 4.0 |
| window: distinct seats | 36.2 | 8.7 | 6.8 |
| dashboard: top versions | 25.0 | 7.4 | 4.4 |
| jsonb: containment | 13.3 | 12.3 | 4.6 |
| **jsonb: containment in a window** | 52.5 | **39.7** | 4.7 |
| btree: project and kind | 0.15 | 0.06 | 0.17 |
| ingest: one row | 4.4 | 1.1 | 1.4 |

Two of those are now wins. The one that stands out is containment in a
window, and taking it apart found something that is not about jsonb at
all:

```
WHERE traits @> '{"plan":"pro"}' AND received_at >= … AND received_at < …    39.7 ms
WHERE received_at >= … AND received_at < … AND traits @> '{"plan":"pro"}'     8.4 ms
```

**The same query, in a different written order, costs 4.7 times as
much.** PG answers both in 5.3 and 5.4 — it orders the quals by
estimated cost, and we evaluate them in the order they were typed.
This is not a jsonb problem: any query whose cheap, selective
predicate is written after an expensive one pays the difference, and
which one a person writes first is a matter of habit.

That outranks the remaining jsonb work by a wide margin, because it is
general. The jsonb items stay on the list behind it:

- `contains` parses BOTH documents on every row, including the
  constant on the right — 200,000 parses of the same small literal for
  one query. There is no place to hold a prepared constant on that
  path today; the compiled expression is where one would go.
- the representation itself, `Value::Json(Cow<str>)`. A field access is
  173 ns a row against PG's 7.5 to 12, and what is left after the
  boundary and key-comparison fixes is the scan and the allocation of
  the result.

## The conjunct-order attack, implemented and reverted

Built as a plan-time pass: stably partition a conjunction into "cannot
raise and is cheap" and "everything else", safe half first, each half
keeping its written order. A partition and not a sort, because moving
something that CAN raise to the front can make an error appear where
the query used to short-circuit past it.

It did what it was built to do — containment written before a
timestamp window went 39.4 ms to 9.2 — and made every indexed shape
slower:

| probe | develop | with the pass |
|---|---|---|
| `project_id = 3` alone (seek) | 1.86 | 1.82 |
| `traits->>'plan' = 'pro'` alone (no seek) | 17.1 | 17.2 |
| both, expensive written first | 5.20 | 6.65 |
| both, cheap written first | 5.99 | 8.24 |

Two controls place the cause. The same worktree built with the pass
compiled but early-returning measures 5.13-5.29 against develop's
5.08-5.50 — so it is the pass, not the binary or its layout. And on a
2000-row copy of the same table **with no indexes** the pass is
faster, 0.201 against 0.554 — so the cost is not a fixed per-query AST
rebuild; it scales with rows, and only where an index is involved.

**The layering was wrong.** PG does not reorder the tree.
`order_qual_clauses` runs in the executor over the quals that are LEFT
after index conditions have been extracted. Reordering the AST before
the seek matcher sees it changes which seek it finds. Reverted on
branch `wt/v7388-qualorder` (`336bd39e`) rather than merged.

The corrected design, for whoever picks this up: order the residual
filter inside the compiled predicate, after the planner has made its
choices. The win is real — 4.3x on the shape that motivated it — and
it is sitting behind one level of the stack.

## Correction — the first measurement under-reported our loss

The harness seeded both databases and measured immediately. PostgreSQL
builds its BRIN summaries in VACUUM, so a freshly loaded table
measures PG with that index doing nothing: the same one-day window
query read 7.3-8.9 ms unsummarised and 1.2-5.1 ms after a
`VACUUM ANALYZE`, on the same rows in the same session.

So every "vs PG" figure taken before 2026-08-21 was flattering us, and
the headline number in this document — **1.6x to 8.9x** — was wrong in
the direction a vendor's own harness must never be wrong. The harness
now vacuums both legs before anything is timed.

Re-measured with that fix, both databases as containers on the same
box (the earlier comparison also had us native against a
containerised PG, which flattered us again), N=4, control leg clean on
all eight cells in both runs:

| shape | 7.38.7 | after v7.38.8 + v7.38.9 |
|---|---|---|
| window: count over a day | 21.2-23.8 ms · **16.4x** | 8.2-9.1 · 6.7x |
| jsonb: containment in a window | 54.1-54.5 · 10.1x | 8.1-12.0 · **1.7x** |
| window: distinct seats | 35.7-38.6 · 5.2x | 11.1-11.6 · **1.6x** |
| dashboard: top versions | 24.6-26.8 · 5.3x | 10.2-14.7 · 2.1x |
| window: group by kind | 22.6-23.2 · 3.7x | 8.4-9.7 · 2.3x |
| jsonb: containment | 12.5-15.8 · 2.4x | 8.6-10.0 · 1.9x |
| ingest: one row | 4.2-5.8 · 2.0x | 3.7-5.9 · 1.7x |
| btree: project and kind | unresolved | unresolved |

The true starting point was **2.0x to 16.4x**, and the worst shape is
still the same one: a range predicate on a BRIN-indexed timestamp does
not drive an index scan for us, so we read every row where PG reads a
fraction. That is the largest single item left and it is not a jsonb
problem.

## The largest remaining item, named: BRIN prunes nothing

The worst shape is a one-day window over a 90-day table — 6.7x behind
after two releases of work, and it was 16.4x. Taking it apart:

```
SELECT count(*) FROM events WHERE received_at >= '2026-06-01' AND received_at < '2026-06-02'

  as the customer's schema has it (BRIN on received_at)      5.09 ms
  with a BTREE added on the same column                      0.105 ms   <- 48x
  PostgreSQL 18, BRIN summarised by VACUUM                   1.2 ms
```

So our index machinery is not the problem: given an index it can use,
we answer this **11x faster than PG**. The problem is that the index
the customer created prunes nothing.

`grep` says it plainly: `BrinSummary { page_index, min_key, max_key }`
is derived and written into a cold-tier segment's sidecar, and **no
query path in `spg-engine` reads it back**. Not for cold data, and for
hot data no summaries exist at all. A BRIN index today costs writes to
maintain and saves no reads.

Two halves to close it, and the second is the one this customer needs:

1. Cold tier: consult the sidecar summaries and skip pages whose
   `(min,max)` cannot satisfy the predicate. The summaries are already
   on disk in the right shape.
2. Hot tier: there is nothing to consult. Summaries have to be
   maintained for hot segments as rows arrive, which is where the
   design work is — and where the write-side cost has to be measured
   rather than assumed, because BRIN's whole appeal is that it is
   cheap to maintain.

Until that lands there is something the customer can do today, and it
belongs in the letter rather than in a backlog: a btree on
`received_at` answers their window queries in 0.105 ms against PG's
1.2. It costs more to maintain than a BRIN — which is presumably why
they chose BRIN — so it is a trade they should make knowingly, not a
recommendation to paper over our gap.

## Sizing the BRIN prize — and a flaw in our own profile

Before building summary maintenance, how much is it worth? The answer
turned on something about the profile rather than about BRIN.

`pg_stats.correlation` for `received_at` against physical order:

```
our profile's events table      0.197     (timestamps cycle: (g % 129600) minutes)
an append-ordered events table  1.000     (what ingest actually produces)
```

At 0.197 every block range spans nearly the whole 90 days, so **no
BRIN implementation can prune anything** — not PG's and not one we
build. Measured on PG18, a one-day window costs 1.43 ms on our profile
and 2.31 ms on the ordered table: BRIN is not what is buying PG its
lead here either.

So the 6.7x on that shape is **not** a pruning gap on this data. Our
4.9 ms is, by the ablation above, almost entirely per-row predicate
work — 21.6 ns a row over 200,000 rows. Building BRIN maintenance and
measuring it on this profile would show nothing, and we would have
concluded BRIN was not worth it.

**On the layout the customer actually has, it is worth a great deal.**
An events table is written in time order. A one-day window over 90
days touches roughly one range in ninety, so the same query would go
from 4.9 ms to the order of 0.06 ms — and we already know what our
machinery does when it has an index it can use: 0.105 ms with a btree,
against PG's 1.2.

Two things follow, and the first is ours to fix before the second:

1. **The profile's data layout is not the customer's.** An events
   table with randomly-ordered timestamps is not a thing that exists.
   The profile has to be re-generated append-ordered, and every figure
   taken against it re-baselined — including the ones already in this
   document and in the letters.
2. Only then is BRIN maintenance worth building, and only then can it
   be measured. `PersistentVec` is a 32-way trie, so the hot tier
   already has natural blocks; a widen-only `(min,max)` per range of
   rows is safe by construction — a summary may over-report and must
   never under-report, which is exactly PG's lossy-index contract.

## Re-baselined on the realistic layout — the prize is now visible

Profile regenerated append-ordered (`pg_stats.correlation` 1.0,
verified), both databases as containers, both vacuumed, N=4, control
leg clean on all eight cells:

| shape | cycling layout | append-ordered |
|---|---|---|
| window: count over a day | 6.7x | **5.7x** (PG 0.997-1.283 ms) |
| window: group by kind | 1.3x | **3.7x** |
| jsonb: containment in a window | 1.7x | **3.7x** |
| window: distinct seats | 1.6x | 2.0x |
| dashboard: top versions | 2.1x | 2.5x |
| ingest: one row | 1.7x | 2.0x |
| **jsonb: containment (no time predicate)** | 1.9x | **1.9x** |
| btree: project and kind | unresolved | unresolved |

Every shape carrying a time window got relatively worse for us, and
the one shape with no time predicate did not move at all. That last
row is the control: it says the change touched what it was supposed to
touch and nothing else. PG is now pruning with the index the customer
created, and we are still reading every row.

The earlier numbers were not wrong about our per-row costs — they were
measured on a table where the customer's index could not help either
engine, which flattered us on exactly the shapes this campaign is
about. This is the honest baseline, and it is the one the letters
should carry.

## Phase B plan for BRIN — written to be executed, not re-derived

Pre-Phase-B gate: passed. On `window: count over a day` our time is
almost entirely per-row predicate over 200,000 rows; pruning to the
one range in ninety that a one-day window touches removes ~99 % of it.
That is far above the double-digit-pp bar.

**Hook points, both found:**

- Storage: `Table.rows()` is a `PersistentVec<Row>` — a 32-way trie,
  so the hot tier already has natural blocks. `crates/spg-storage/src/table.rs:793`.
- Executor: the sequential filter loop is
  `crates/spg-engine/src/select.rs:6685`, `for i in 0..n` over a run
  cursor. Pruning replaces the range it iterates.

**The invariant that makes it safe:** a summary may over-report and
must never under-report. So maintenance is WIDEN-ONLY — an insert
widens its range's `(min,max)`, an update widens, and a delete leaves
it alone. A range left wider than necessary is correct and merely less
selective, which is exactly PG's lossy-index contract (it rechecks).
Nothing can ever skip a matching row.

**Steps:**

1. `IndexKind::Brin` carries `summaries: Vec<(i64, i64)>`, one entry
   per `RANGE_ROWS` (start at 1024) of physical slots. Insert extends
   or pushes; update widens the containing entry. O(1) either way.
2. `Table::brin_candidate_slots(col_pos, lo, hi) -> Option<Vec<Range<usize>>>`
   — `None` when there is no BRIN index on that column, so every
   caller that does not know about BRIN keeps its current behaviour.
3. The scan loop asks for candidates when the WHERE carries a range
   predicate on a BRIN-indexed column, and iterates those slots
   instead of `0..n`. The predicate still runs on every surviving row:
   the summary decides what to skip, never what to return.
4. Persistence: summaries are derivable from the rows, so on load they
   can be rebuilt in one pass rather than serialised. Cheaper to get
   right than a new on-disk field, and it cannot go stale.
5. Measure on the append-ordered profile — and on the cycling one as a
   NEGATIVE control, where correlation 0.197 means a correct
   implementation must show no improvement at all. A version that
   speeds THAT up is skipping rows it should not.

Step 5 is not optional. It is the only cheap check that separates
"prunes correctly" from "prunes too much", and a wrong implementation
looks like a bigger win.

## BRIN landed — where the profile stands now

Both as containers, vacuumed, N=4, control leg clean on all eight
cells:

| shape | v7.38.10 | with the prune |
|---|---|---|
| window: count over a day | 5.7x behind | **unresolved** (1.04-1.69 vs 0.98-3.09) |
| window: group by kind | 3.7x | **unresolved** |
| window: distinct seats | 2.0x | **FASTER by 3.9 %** |
| btree: project and kind | unresolved | unresolved |
| jsonb: containment | 1.9x | 1.8x |
| dashboard: top versions | 2.5x | 2.1x |
| ingest: one row | 2.0x | 2.1x |
| **jsonb: containment in a window** | 3.7x | **4.1x** |

Three window shapes reached parity or better; the campaign's worst
cell — 16.4x at the start — is now unresolved against PG.

**The last row did not move, and the reason is not a miss.** That
query's `traits @> …` is answered by the GIN index, so its rows arrive
from the index and never reach a scan; there is no slot list to prune.
PG combines the two indexes into one bitmap and we do not. That is a
different optimisation — an AND of two index results — and it is now
the largest single cell on the profile. It is not a regression and it
is not BRIN's to fix.

## Next

Phase A decomposition against PG18's scan path, per
`perf-decomposition-vs-polish` — the two causes above are entry
points, not a diagnosis. The Pre-Phase-A gate is satisfied: the gap is
measured against the reference the customer actually runs, on a box
whose noise floor was measured in the same run, and it reproduces on a
second machine.
