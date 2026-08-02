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

## 5. Recommendation

Adopt (a). Sequence: converge comparison (with a bench) → thread the
collation name from parser into `ColumnDef` and the catalog → wire
`icu_collator` behind the stored collation, defaulting to C so nothing moves
until a database declares otherwise → survey which locales `compiled_data`
covers → index rebuild path.

Step one is worth doing on its own merits whatever happens to the rest.
