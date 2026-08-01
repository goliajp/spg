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

## 5. Recommendation

Adopt (a). Sequence: converge comparison (with a bench) → thread the
collation name from parser into `ColumnDef` and the catalog → wire
`icu_collator` behind the stored collation, defaulting to C so nothing moves
until a database declares otherwise → survey which locales `compiled_data`
covers → index rebuild path.

Step one is worth doing on its own merits whatever happens to the rest.
