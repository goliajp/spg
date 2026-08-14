# `filtered then order` — Phase A, and what the 1.8-2.1× actually is

**Context.** The release-blocking sweep (`scripts/perf-endpoint-sweep.sh`)
reports 20 of 32 cells losing to PG18, with `control_false_differences=0` —
a zero noise floor, so every verdict sits outside the instrument's own
resolution. `filtered then order` loses at every size: 2.11× at 10k, 1.83× at
50k, 1.81× at 400k. Its shape is
`SELECT pad FROM t WHERE id % 3 = 0 ORDER BY k`, and both engines plan it
identically — `Sort ← Seq Scan + Filter` — so the loss is execution, not
planning.

All figures below: 50,000 rows, `psql`'s own `\timing`, min of three
executions per invocation, three interleaved rounds per leg, one client
binary for both legs.

## The split

| cell | SPGS | PG18 | |
|---|---:|---:|---:|
| deliver 50k rows, no filter, no sort | 4.301 | 4.159 | **1.03×** |
| narrow column, sort | 5.894 | 6.142 | **0.96×** |
| narrow column, sort by PK | 4.924 | 4.971 | 0.99× |
| `count(*)`, no predicate | 0.239 | 0.863 | **0.28× — we win** |
| `count(*) WHERE id > 0` | 0.806 | 1.985 | **0.41× — we win** |
| `count(*) WHERE id % 3 = 0` | 3.328 | 1.105 | **3.01×** |
| `count(*) WHERE id + 0 = id` | 3.099 | 1.447 | 2.14× |
| `count(*) WHERE k % 3 = 0` (non-PK) | 3.316 | 1.233 | 2.69× |
| wide payload, sort, deliver 50k | 13.738 | 10.011 | 1.37× |

Read across, three things fall out and two of them corrected an earlier
reading of the same data:

1. **Row delivery is at parity.** 1.03× for fifty thousand rows carrying a
   60-byte payload. Encoding and sending is not where this goes.
2. **Sorting is at parity** — 0.96× and 0.99× on narrow columns. An earlier
   subtraction (`wide sort` minus `wide deliver`) had attributed 1.6× to the
   sort; measuring the sort on its own says otherwise. What costs 1.37× in
   the wide cell is the payload width, not the ordering.
3. **We are FASTER than PG on a bare count (0.28×) and on a simple
   comparison predicate (0.41×)**, and 3× slower the moment the predicate
   contains arithmetic.

The SPGS column is internally comparable regardless of what PG's planner
chose: 0.239 → 0.806 → 3.328. Adding `% 3` to a predicate costs **+50 ns per
row** where the comparison alone costs ~11 ns. One integer modulo is a few
cycles; fifty nanoseconds is about a hundred and fifty.

It is not modulo-specific — `id + 0 = id` costs the same. Any arithmetic in
the predicate does it.

## Why

`crates/spg-engine/src/eval/compiled.rs:296`. The compiled expression VM
recognises one three-step shape:

```rust
let [Step::Column(pos), Step::Lit(lit), Step::Binary(op)] = &self.steps[..] else {
    return None;
};
if !matches!(op, BinOp::Eq | BinOp::NotEq | BinOp::Lt | BinOp::LtEq | BinOp::Gt | BinOp::GtEq) {
    return None;
}
```

`column <comparison> literal`, and nothing else. `id > 0` is exactly that
shape and runs at 11 ns/row. `id % 3 = 0` compiles to five steps —
`[Column, Lit(3), Binary(%), Lit(0), Binary(Eq)]` — misses the pattern, and
falls into the general step loop at 50 ns/row.

So the predicate does compile (`fully_compilable` accepts every binary
operator, and only bitwise/inet ops route to `Step::Subtree`). The cost is
not interpretation of the tree. It is that the general VM loop costs ~10 ns
a step where the special case costs ~11 ns for the whole predicate.

## Attack candidates, in the order the measurement supports

1. **Make the general step loop cheaper.** Five steps at 10 ns each is the
   whole gap, and it is the broad fix: every predicate that is not
   `column <cmp> literal` pays it, which on this corpus is most of them.
   Wants a leaf-symbol profile first — the per-step cost has not been
   attributed, and guessing at it is what this campaign keeps punishing.
2. **Widen the recognised shape** to `column <arith> literal <cmp> literal`.
   Narrow, mechanical, and it covers the bucketing and parity predicates
   that show up in real schemas. Worth doing only if (1) turns out to be
   architectural — a second special case is a worse answer than a faster
   loop.
3. **Wide-payload sort, 1.37×.** Separate from this document's subject and
   already an area with history (the v7.37.13 sort projection pruning).

## What this does NOT say

No profile has been taken. The 10 ns/step figure is arithmetic on wall-clock
differences, not an attribution to any line inside the loop. Phase B does not
start until a profile says which part of a step costs what — this campaign
has already refuted three code-read hypotheses by measurement, and the step
loop is a place where "it must be the Value moves" would be the fourth.
