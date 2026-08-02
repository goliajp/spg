# Locale collation — RFC

SPG orders text by bytes. That is the C collation, it is what
`pg_database.datcollate` advertises, and it is self-consistent — but a
customer's PG is very often `en_US.utf8`, and against one of those, nine
ordinary query shapes disagree with no error raised. One of them disagrees
about which ROWS come back, not merely their order: `WHERE name BETWEEN 'B'
AND 'c'` returns four rows here and one there.

Worse, a dump declaring `en_US.utf8` restores clean. `CREATE DATABASE ...
LC_COLLATE`, `CREATE TABLE ... COLLATE`, `ALTER ... COLLATE` and
`CREATE COLLATION` are all accepted; only `CREATE INDEX ... COLLATE`
refuses. The declaration is taken and ignored.

Tracked as F36. Written round 671.

Everything below was measured, on this tree, against the PG18 oracle. No
route was rejected on reputation.

## 1. What has to be reproduced

Derived from the oracle's behaviour, not from anyone's source (clean-room):

| probe | PG18 `en_US.utf8` | rule it implies |
|---|---|---|
| `'a' < 'A'` = t, order `a,A,b,B` | | primary weight ignores case; tertiary puts lowercase first |
| `e,E,é,ê,f` and `'résumé' < 'resumes'` = t | | accents are a SECONDARY weight; primary treats `é` as `e` |
| ` ,_,1,a,A` | | space < underscore < digit < letter |
| `a b` \| `a-b` \| `ab` \| `aB` | | space and hyphen carry weight; they are not ignorable |
| `a1,a10,a2` | | no numeric-aware ordering |
| `z,Z,あ,中` | | Latin < Kana < Han |

That is a three-level weighted comparison — the UCA shape.

## 2. Routes

### (b) libc `strcoll` — REJECTED, on two measured grounds

`spg-sql`, `spg-storage` and `spg-engine` all carry `#![no_std]` and every
dependency is `default-features = false`. `strcoll` needs std.

The second reason is worse than the first: `strcoll` orders by the locales
INSTALLED ON THE HOST. A database whose `ORDER BY` depends on which machine
it runs on is not a database. Rejected regardless of the no_std question.

### (a) ICU4X `icu_collator` — RECOMMENDED, and the evidence is a build

Tested in a throwaway crate rather than argued about:

* `icu_collator 2.2.1`, `default-features = false`, `features = ["compiled_data"]`
* **compiles under `#![no_std]`** — a `#![no_std]` lib crate calling
  `Collator::compare` builds clean.
* **reproduces all seven probes above, exactly** — `a,A,b,B` /
  `a b|a-b|ab|aB` / ` ,_,1,a,A` / `e,E,é,ê,f` / `z,Z,あ,中`, and the three
  boolean shapes.
* data payload `icu_collator_data` ≈ 477 KB compressed.

### (c) self-implemented UCA/DUCET subset — NOT NEEDED

Was the fallback if (a) failed either the no_std or the fidelity test. It
failed neither, so building and carrying a DUCET table is work with no
remaining justification. `unicode-normalization` is already a dependency
(the NFD step UCA needs), so this stays cheap to revisit if (a) ever has to
be dropped.

## 3. Prerequisite: converge text comparison first

Text is compared in at least six independently written places —
`orderby.rs:541`, `binop.rs:1098`, `binop.rs:5690`, `binop.rs:5821`,
`eval.rs:864`, plus `spg-storage`. Swapping a collator into six sites has
the failure mode round 664 measured on the sum/avg family: the guard goes on
some of them, the others keep the old behaviour, and only a differently
shaped query finds out.

So: converge to one comparison, the way round 665 converged four
accumulators into `acc_cell`, and only then change what it does. Round 665
also recorded the trap to avoid — extracting that matrix out of its call
sites once cost 23x on a scan (`column_accepts`'s `#[inline]` comment), so
the converged comparison needs the same treatment and a bench.

## 4. Open, and each is its own decision

* ~~**Which collations.**~~ **Answered in round 680: all 880.** The list was
  taken off PG18's `pg_collation` verbatim and every name fed to
  `collate::compare`. First run: **877 of 880**. The three misses each had a
  reason and all three are now covered — `C.utf8` is the C collation wearing
  an encoding suffix, and `unicode` / `pg_unicode_fast` are PG18's names for
  the UCA root, which is ICU's `und`. The survey is a test rather than a
  note, and it asserts an empty failure list rather than a percentage, so a
  future ICU upgrade that drops a locale fails loudly.
* **Index order.** Collation determines index key order. Changing it
  invalidates every on-disk text index, so this needs a rebuild path and a
  data-compat story, not just a comparison swap.
* **What the declaration means.** Today `CREATE TABLE t(x TEXT COLLATE
  "en_US")` is accepted and ignored. Once the collator exists the
  declaration has to be stored per column and honoured.

  Corrected in round 676, having been stated wrongly here and in two other
  places: `ColumnDef` DOES carry `collation: Collation` and
  `collation_explicit: bool`. What it does not carry is the NAME. `Collation`
  is a two-variant enum — `Binary` and `CaseInsensitive` — built for MySQL's
  `utf8mb4_bin` / `_general_ci` distinction, and `from_collation_name` folds
  everything without a `_ci` suffix into `Binary`. So `COLLATE "C"`,
  `COLLATE "POSIX"`, `COLLATE "en_US"` and `COLLATE "default"` all arrive as
  the same value and cannot be told apart afterwards.

  The name therefore has to survive CREATE TABLE, the persisted schema and
  the catalog read. That is a `ColumnSchema` field plus a FILE_VERSION
  appendix — the sparse index-aligned kind this codec already uses several
  times over, costing two bytes for a table that declares none.

## 4b. Threading — measured in round 681, not yet built

Three findings, each of which narrows the job:

**Equality is untouched.** PG18's `en_US.utf8` is a deterministic collation
(`collisdeterministic` is true). Measured: `'a' = 'A'` is false, `DISTINCT`
over `'a','A'` gives two groups, and a join on them matches nothing. A
locale collation changes ORDER and nothing else, so every comparison site
that only asks about equality — join keys, DISTINCT, GROUP BY — needs
nothing at all.

**Sort keys would have avoided threading entirely, and are not available.**
`icu_collator` can emit a byte string whose byte order IS the collation's
order (`write_sort_key_to`). Built once where the column is known, it would
leave all 47 downstream comparisons the byte compares they already are, and
cost O(n) instead of O(n log n) collator calls. It is behind the crate's
`unstable` feature with an upstream graduation tracking issue. Sort keys
would become the on-disk order of every text index; an API upstream says may
still change is the wrong foundation for bytes that outlive the process.
Revisit when it graduates.

**Round 682 tried to build it and reverted.** Three attempts, each aimed at
a place the collation "obviously" had to pass through, and none of them was
on the path a plain `SELECT loc FROM t ORDER BY loc` actually takes:

  * `sort_by_keys` / `cmp_multi_key` (the OrderKey family) — wired, no effect.
  * the four `order_by_value_cmp_in` sites in `select.rs` — wired, no effect.
  * `describe.rs` dropping the collation when projecting a bare column
    reference — a real bug, fixed, still no effect.

What settled it was not reading more code. Forcing `collate::compare` to
reverse EVERY comparison and watching the output not move proved the
resolver was never called at all. The path is `run_single_table_scan`, whose
top-N trim calls `cmp_multi_key` directly, and two more callers sit in
`locks.rs` and `join.rs` — the latter inside an `Ord` impl with no context
to look a column up from.

So the honest shape of this step: it is not "one enum and its comparison".
It is every sort path in the engine, one of which cannot reach a schema at
all without changing what it stores. That is a design change to how sort
keys carry their metadata, and it needs to be designed before it is typed —
the three attempts above were typed first.

The revert was to the round-680 tree. Nothing half-wired was left behind,
because a collation honoured on some paths and not others is worse than one
honoured nowhere: it would make ORDER BY depend on which plan the optimiser
picked.

**The seam was thought to be `OrderKey`.** `orderby::OrderKey` is
the repo's own sort-key abstraction — `Num(f64)`, `Int(i128)`,
`Text(String)`, `Bytes(..)`, and NULL sentinels — built at the point where
the ORDER BY expression and the row's columns are both in hand, then
compared by `cmp_multi_key` / `sort_by_keys`. A text key carrying its
column's collation is a change to one enum and its comparison, not to 47
call sites. There are 33 construction sites; only those that know the column
can supply a collation, and the rest keep today's byte order, which is
correct for a column that declares none.

## 4c. What actually honours a collation — measured, round 685

Rounds 683-684 wired the single-table scan and its commit says "COLLATE now
changes the order". Measured across query shapes afterwards, that is true of
two:

| shape | honours COLLATE |
|---|---|
| `SELECT loc FROM t ORDER BY loc` | yes |
| `SELECT DISTINCT loc FROM t ORDER BY loc` | yes |
| `SELECT a.loc FROM a JOIN b … ORDER BY a.loc` | **no** |
| `SELECT loc FROM t GROUP BY loc ORDER BY loc` | **no** |

Round 685 then repeated round 682's mistake: seven more sites were wired
across `select.rs` before checking whether the failing queries reach them.
Forcing every collated comparison to reverse moved nothing, proving none of
the seven is on those paths. Reverted, again, to the committed tree.

Two things this establishes for whoever does the join and group-by paths:

**Probe shapes matter more than probe count.** The first survey wrapped
every case in `string_agg(loc, ',' ORDER BY loc)`, whose final order comes
from the aggregate's own sort, not the query's. Six shapes looked broken
that were never being measured. Bare `SELECT … ORDER BY` is what tells the
truth.

**Force-reverse before wiring, not after.** It costs one build and answers
"is this code on the path" exactly. Both rounds that skipped it wired the
wrong places; the round that used it first landed in one go.

**`TopNEntry` in `join.rs` is not the join sort.** It is the top-N heap,
reached only with a LIMIT. A plain ORDER BY over a join sorts somewhere
else, and that somewhere has not been located yet.

## 4d. Round 686 — GROUP BY landed; the join sort is still unlocated

`GROUP BY loc ORDER BY loc` now matches PG. Located by forcing each
candidate to reverse and watching which one flipped: `aggregate.rs`'s
`sort_synth_by_order_by`, not any of the eleven places rounds 682 and 685
wired on a guess.

Landing it took two changes, and the second is the interesting one. Wiring
the comparator was not enough, because a GROUP BY key does not keep its
column: the aggregate builds a synthetic schema of `__grp_0..K`, and the
resolver looked the key up there and found no collation. The fix is beside a
precedent — the enum-order work already carries `user_enum_type` onto the
synthetic column for exactly this reason. A collation travels the same way.

**Anything a downstream sort needs about an original column has to be
carried onto the synthetic one.** That is the shape to check first for the
remaining path.

The join sort is NOT `partial_sort_tagged` in `exec_joined_select`. Round
685 wired that on a guess; round 686 force-reversed it and `SELECT a.loc
FROM a JOIN b … ORDER BY a.loc` did not move. It is also neither of the two
comparison families — reversing each of those whole left it unchanged. So a
plain ORDER BY over a join sorts somewhere a census of `value_cmp` and
`cmp_multi_key` callers does not reach, and that is where to look next.

## 4e. Round 687 — the join path, located exactly

`SELECT a.loc FROM a JOIN b … ORDER BY a.loc` sorts by bytes. Three facts,
each proven rather than reasoned:

1. **`build_combined_schema` in `join.rs` IS on the path, and DOES need the
   collation.** Proven by panicking inside it and watching the query hit the
   panic. It builds each qualified column with `ColumnSchema::new`, which
   knows only name, type and nullability, so the source column's collation
   stops there.

2. **Fixing only that is not enough.** With the collation carried across the
   combined schema, the query still sorted by bytes — the projection drops
   it one layer later.

3. **`ProjectedItem` is where it is dropped, and the fix has two
   precedents.** That struct already carries `user_enum_type` and
   `mysql_fsp` for exactly this reason, and its own doc comments say so: "a
   projection that dropped this made the RESULT schema forget it — and a
   UNION's combined ORDER BY … silently fell back to TEXT order". Collation
   is the third thing living outside the DataType lattice. Six sites rebuild
   a `ColumnSchema` from a `ProjectedItem` and each copies those two fields
   by hand.

So the remaining work is: add `collation_name` to `ProjectedItem`, fill it
in `build_projection` beside `user_enum_type` (four sites), and copy it at
the six rebuild sites. Round 687 attempted this and made three mechanical
editing errors on a 9000-line file, so `select.rs` was reverted; the two
`join.rs` and `aggregate.rs` findings above stand on their own evidence.

**The pattern, now three-for-three.** GROUP BY's `__grp_j` columns (round
686), the join's qualified columns, and the projection's output columns are
all schemas rebuilt for an intermediate result, and all three silently lost
the collation. Whenever a `ColumnSchema` is constructed rather than cloned,
check what the original carried.

## 4f. Round 689 — the full shape list, re-measured

Round 688 verified four shapes and reported the ordering side closed. F36
originally measured nine. Re-running the fuller set says three of seven pass
and four do not, so "closed" was said on a subset:

| shape | SPG | PG18 | |
|---|---|---|---|
| `ORDER BY loc` | en_US | en_US | ok |
| index scan `WHERE loc > 'A' ORDER BY loc` | en_US | en_US | ok |
| **`WHERE loc BETWEEN 'B' AND 'c'`** | **4 rows** | **1 row** | **different DATA** |
| `min(loc)` / `max(loc)` | Banana / Ápple | apple / Zebra | wrong |
| `row_number() OVER (ORDER BY loc)` | bytes | en_US | wrong |
| `ORDER BY upper(loc)` | bytes | en_US | wrong |
| `'a' < 'b' COLLATE "en_US.utf8"` | refused | t | wrong |

BETWEEN is the serious one: it is not an ordering difference, it is a
different row set from the same SQL. It goes through `binop::compare`,
proven by panicking in that function's Text arm and watching the query hit
it.

`compare(op, l, r)` takes two values and no column, exactly as the sort
comparators did. Two things make it harder than the sort was. Its own
comment records it as the dominant cost of a scan — 35.6% of self time on
`g = 5` — so a parameter added here has to survive a bench, not just a
correctness gate. And a comparison's operands are two arbitrary expressions,
so "which column's collation applies" is a resolution question PG answers
with collation derivation rules that SPG does not model at all.

`ORDER BY upper(loc)` is the same question in the sort: PG gives a function
result the collation of its argument. SPG resolves a collation only for a
bare column reference, which is why that row is wrong.

So the remaining work is not more wiring. It is collation DERIVATION — what
collation an expression has — and that is a piece of design, not an edit.

## 4g. Round 690 — min/max and the window, and a tie broken by bytes

Two of the four residuals from 4f were bare column references, so neither
needed derivation. Both are now PG18-verified.

`min` / `max` fold a running extreme rather than sorting rows, so round
688's sort work did not reach them: `min(loc), max(loc)` gave
`Banana | Ápple` where PG18 gives `apple | Zebra`. The fix carries the
argument's collation on `AggSpec`, beside `enum_labels` — both are facts
about the ARGUMENT that the comparison cannot look up for itself.

Two things that round measured and are worth keeping:

* The resolver first went INSIDE the enum-label loop, which is guarded by
  "the catalog holds at least one enum type". With no enum in the database
  it never ran, and the answer did not move. A collation has nothing to do
  with enums; it needed its own unconditional pass. The wrong answer looked
  exactly like a wrong code path, which is what the force-reverse discipline
  is for — the path was right, the guard was not.

* The fused aggregate lane sends an ENUM argument to the generic path,
  because it does not carry member order. A collation could have taken the
  same exit and did not: `FusedOp::Extreme` carries it, so a collated column
  keeps the shard-parallel scan.

A window's ORDER BY sorts a key tuple it builds per row. The collation is
resolved from the key's already-bound column position and passed to a new
`order_key_cmp_in`. Only the SORT takes it: peer detection asks whether two
keys are EQUAL, and under a deterministic collation byte equality gives the
same verdict, so the peer scans keep the cheaper comparison.

That last point turned out to need something. PG18's `en_US.utf8` has
`collisdeterministic = t`, which means a collation TIE is broken by the
bytes, and it is observable: `e` + U+0301 and U+00E9 are canonically
equivalent, so ICU calls them Equal, but PG18 orders the decomposed form
first (0x65 < 0xC3) and reports `=` as false. `collate::compare` now appends
that tiebreak. Without it two canonically-equivalent values sorted in
whatever order the sort happened to leave them in.

Remaining: `BETWEEN` and `ORDER BY upper()` — both derivation, as 4f
records — plus the explicit expression `COLLATE`, which is refused at parse
time. That one needs no derivation (the name is written in the query) but it
is currently DROPPED by the expression parser, so honouring it means finding
it a home. `ast::OrderBy` already carries `desc` and `nulls_first`, which
are ordering-only facts of the same kind; a new `Expr` variant would instead
put a new arm on `eval_expr`, which this repo has measured to overflow the
debug stack.

## 4h. Round 691 — the explicit `COLLATE`, and where a name can live

`ORDER BY loc COLLATE "en_US.utf8"` was refused at parse time. It needed no
derivation — the name is written in the query — so what was missing was
somewhere to PUT it. The expression parser consumed the clause and dropped
it, and by the time the sort ran there was nothing left to honour.

It went on `ast::OrderBy`, beside `desc` and `nulls_first`. At an ORDER BY
key a collation IS ordering information, and nothing downstream of the sort
wants it. The alternative was an `Expr::Collate` variant, which would put a
new arm on `eval_expr` — measured in this repo to overflow the debug stack,
and recorded as such.

The parser reaches the field through a save/restore channel, the same shape
it already uses for `pending_sample_preds`, active only while an ORDER BY
key is parsing. Four key parsers share it: the statement's ORDER BY, `WITHIN
GROUP`, and an aggregate's own ORDER BY; MySQL's synthesised rollup order
passes `None`, having no source text to have written one in.

Two boundaries the round drew deliberately:

* Outside an ORDER BY key an unperformable collation still ERRORS. A
  comparison cannot be fixed by carrying a name — `binop::compare` takes two
  values, and which operand's collation applies is the derivation question.
  Accepting the clause there and ignoring it would be F36's own defect.

* An explicit name this build cannot perform is REFUSED, not dropped. A name
  a COLUMN declares is treated differently: that table already exists, the
  declaration was warned about at DDL time, and failing every query against
  it would be worse than ordering by bytes.

Verified against PG18 on a column declaring nothing: the explicit name
orders the key, an explicit `C` stays byte order, and an explicit name beats
a column's own declaration.

Remaining after this: `BETWEEN` and `ORDER BY upper()`, both derivation.

## 4i. Round 692 — derivation, and the warning that had gone stale

`ORDER BY upper(loc)` needs to know what collation an EXPRESSION has, which
is a rule set, not a wire. Measured off PG18 with `collation for (…)`:

| expression                    | PG18 reports                 |
|-------------------------------|------------------------------|
| `upper(a)`, a is en_US.utf8   | `"en_US.utf8"`               |
| `upper(plain)`, undeclared    | `"default"`                  |
| `a \|\| 'literal'`          | `"en_US.utf8"`               |
| `'literal'`                   | (none)                       |
| `a COLLATE "C"`               | `"C"`                        |
| `a \|\| b`, en_US and C     | (none) — and USING it errors |

`crates/spg-engine/src/collate_derive.rs` holds them: a four-state
`Derived` (None / Implicit / Explicit / Conflict) and a `combine` that is
PG's precedence. `ORDER BY a || b` over two differently-collated columns now
fails with PG's own sentence, `collation mismatch between implicit
collations "en_US.utf8" and "C"`. Picking a winner there would have been
easy and would have been F36's defect in a new place: it silently changes
the ORDER of a query the user believes is well-defined.

Two things the round turned up that were not in the plan:

* **`ORDER BY a COLLATE "C"` was still absorbed.** Round 691 recorded a
  name into the lowering channel only when the OLD allow-list rejected it,
  and `C` was on that list — it had been a no-op since before a column could
  declare anything. Once a column can, absorbing the clause means the
  COLUMN's collation wins where the query asked for bytes. Every name goes
  to the channel now. This surfaced because a pin used a table whose first
  column declared en_US; the round-691 pins used one that declared nothing,
  where byte order and the absorbed clause agree.

* **The DDL warning had become false.** It read "SPG records the declaration
  but orders this column by bytes … ORDER BY, min/max and range comparisons
  will not follow it". Two thirds of that stopped being true in rounds
  683–690. It now names what IS still true — range comparisons — and a
  separate message covers a name this build cannot perform at all. A stale
  warning is worse than none: a customer reads it and plans around it.

The one row above SPG does not match is `upper(plain)`: PG's container has
`datcollate = en_US.utf8`, so an undeclared column inherits en_US there,
while SPG's database default is C. That is the database-level default, not a
derivation rule, and it is the same difference `ORDER BY plain` has always
had.

Remaining: the range comparison (`BETWEEN`, `<`, `>`). Measured on PG18,
that is the whole of what is left — equality, `LIKE`, `IN` and
`count(DISTINCT …)` are all unaffected by a collation, and `least`/`greatest`
follow the ordering operators. It goes through `binop::compare`, whose own
comment records it as 35.6 % of self time on a scan, so it needs a design
that resolves the collation ONCE per predicate rather than per row, and a
bench beside the correctness gate.

## 4j. Round 693 — the range comparison, and where it did NOT go

`loc BETWEEN 'a' AND 'd'` returns a different ROW SET, not a different
order: PG18 under en_US.utf8 gives `apple, Ápple, Banana, cherry`, and byte
order drops two of those. This was the last shape.

The interesting part is where it landed. §4f said `binop::compare` "takes
two values and no column" and records itself as 35.6 % of a scan's self
time, so a parameter there had to survive a bench. It never got one. The
first attempt DID put a hook in `eval_expr`'s Binary arm, which looks like
the obvious home — and a panic planted in it was never reached, because a
WHERE predicate does not go through the tree evaluator at all. It compiles
to a step VM.

That turned out to be the better seam. The predicate COMPILER already bails
a subtree out to the tree evaluator when an operand witnesses an enum, with
a comment noting the check is compile-time and costs nothing when the
catalog has no enum types. A collation takes the same exit: an operand that
derives one sends its subtree out, once, while the predicate compiles. A
column that declares nothing never leaves the VM, so the scan pays nothing
per row — the hot path is untouched rather than optimised.

Bench, with a control, because a bare "within noise" is not evidence:
`cargo bench -p spg-engine --bench execute` moved between -3.8 % and +3.5 %
against its stored baseline. Re-running the IDENTICAL binary immediately
afterwards moved between -8.8 % and +1.3 %. The machine's own run-to-run
drift is wider than the change, so there is nothing to see; the control is
what says so.

`least` / `greatest` follow the ordering operators, and needed their own
edit for a structural reason, not an oversight: their witness is the
argument's column, so like the enum case they cannot be answered from the
value-level function dispatch.

Measured on PG18, that is the complete list. `=`, `<>`, `LIKE`, `IN` and
`count(DISTINCT …)` are unaffected by a collation — its `en_US.utf8` is
deterministic, so equality is byte equality there too.

### What remains after 693

* **Index order.** A collation decides a text index's key order, so changing
  one invalidates every text index on disk. Needs a rebuild path and a
  data-compat story; unchanged since §4.
* **The database default.** SPG's `datcollate` is C. The oracle container's
  is `en_US.utf8`, which is why a column declaring NOTHING still sorts by
  locale there and by bytes here. That is a database-creation-time property,
  not a derivation rule, and it is the only difference the shape list still
  shows.

## 5. Recommendation

Adopt (a). Sequence: converge comparison (with a bench) → thread the
collation name from parser into `ColumnDef` and the catalog → wire
`icu_collator` behind the stored collation, defaulting to C so nothing moves
until a database declares otherwise → survey which locales `compiled_data`
covers → index rebuild path.

Step one is worth doing on its own merits whatever happens to the rest.
