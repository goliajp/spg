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

## The profile — where the step loop's time actually goes

Phase A stopped here on purpose: the ~10 ns a step was arithmetic on wall
clock, not an attribution. A leaf-symbol profile of each predicate,
separately, with `sample` over `probe_pred_vm`
(`crates/spg-engine/examples/probe_pred_vm.rs`, built `--profile
release-dbg`):

**`WHERE id % 3 = 0` — 6,000 reps**

| leaf | samples |
|---|---:|
| `core::ptr::drop_glue<spg_storage::Value>` | **1983** |
| `eval::compiled::run_compiled_steps` | 1939 |
| `eval::binop::apply_binary_by_ref` | 1228 |
| `eval::binop::mod_op` | 1224 |
| `run_single_table_aggregate` | 901 |
| `eval::binop::compare` | 385 |
| `Value::clone` | 355 |
| `Value::data_type` | 243 |

**`WHERE id > 0` — 30,000 reps (five times as many)**

| leaf | samples |
|---|---:|
| `run_single_table_aggregate` | 1771 |
| `eval::compiled::eval_compiled_pred` | 1629 |
| `eval::binop::apply_binary_by_ref` | 1528 |
| `eval::binop::compare` | 1336 |
| `Table::header_visible` | 636 |
| `predicate_is_true` | 612 |
| `drop_glue<Value>` | 450 |
| `run_compiled_steps` | — absent |

Normalised per rep, `drop_glue` is **22× heavier** in the arithmetic
predicate (0.331 samples/rep against 0.015), and `run_compiled_steps` does
not appear in the control at all — the three-step special case bypasses the
step machine entirely, which is the mechanism Phase A inferred and this
confirms.

**The largest single leaf is destroying `Value`s, ahead of the modulo it is
carrying.** The step machine materialises an intermediate `Value` per step —
`id % 3` becomes one, pushed, popped, compared, dropped — and `Value` has
heap-carrying variants, so its drop glue runs on every one of them.

So the cost is not the arithmetic and not interpretation of a tree. It is
`Value` construction and destruction churn, per row, in the general loop.

## Attack candidates, in the order the measurement supports

1. **A typed lane for scalar arithmetic chains.** Compile integer-only
   arithmetic into steps that operate on `i64` without materialising a
   `Value` between them, falling back to the general machine for anything
   else. This is what the existing three-step case already does in spirit —
   it reaches the answer without touching the stack — generalised from one
   hard-coded shape to a class.
2. **Widen the recognised shape** to `column <arith> literal <cmp> literal`.
   Mechanical, covers bucketing and parity predicates, and strictly worse
   than (1) as an answer: a second hard-coded shape rather than a machine
   that stops churning. Worth it only if (1) proves architectural.
3. **Wide-payload sort, 1.37×.** Separate subject, separate history (the
   v7.37.13 sort projection pruning).

Phase B may now start on (1): the target is named and attributed, which is
what this document required of it.

## What this still does NOT say

How much of `drop_glue` is recoverable. A typed lane removes the
intermediates it covers; it does not remove the row's own values, and the
control shows 450 samples of drop glue on a predicate that never enters the
step machine. The number to beat is the difference, not the total — and it
should be measured on the same harness before and after rather than
predicted.
