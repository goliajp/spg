# Collation: the design, and the order it has to be built in

*Decided 2026-08-23. Supersedes the "three options" in
`FINDING-2026-08-23-database-collation.md`, which asked the question.*

## The question

SPG's database collation is fixed at `C`. A stock PostgreSQL is
`en_US.utf8`. So every text column that does not carry its own `COLLATE`
sorts by bytes in SPG and by locale in the database it replaces —
silently, on `ORDER BY`, `min`/`max`, and every range comparison.

The finding listed three ways to close it and said the choice was a
decision rather than a fix, because they differ in what they do to data
already on disk. This is that decision.

## What the oracle actually does

Measured on PostgreSQL 18.4, because the shape of the answer depends on
it:

| | |
|---|---|
| An undeclared column's `information_schema.collation_name` | **NULL** — it inherits, it does not carry |
| `ALTER DATABASE … LC_COLLATE` | **refused** — immutable after creation |
| Where `datcollate` comes from | the environment at creation (`LANG=en_US.utf8` in that container), overridable per `CREATE DATABASE` |
| Same query with and without an index on a collated column | **identical** |

The third line is the whole design: PostgreSQL does not have a "change
the collation of a live database" problem, because it does not allow
one. Neither will SPG. That removes the reindex hazard that made this
look like a hard decision — the hazard was in option 2 and option 3, and
both are avoidable.

The fourth line is a defect SPG has today, and it comes first.

## The layers, in build order

### S0 — an index key carries its column's collation

**This is a live silent-wrong-answer bug, independent of everything
below.** A column declared `COLLATE "en_US.utf8"`, five rows, `WHERE x >
'b'`:

| | rows |
|---|---|
| PostgreSQL 18.4, with or without an index | `Bob client DateStyle Zebra` |
| SPG, no index | `Bob client DateStyle Zebra` |
| SPG, with an index | `client` |

Three rows gone. `collate::column_key_is_bytewise` asks the DIALECT and
the `Collation` enum and never asks `collation_name`, so a PG column
with a locale collation takes the byte-keyed seek while the predicate
means the locale. The function's own documentation is about exactly this
failure — *"Answering `false` here costs a scan; the alternative cost
rows"* — and it was written for the MySQL case only.

Refusing the seek would fix it and cost a scan. That is the wrong fix
here, because S2 makes locale collations the common case and SPG's perf
line does not permit turning every text index into a scan.

So the key carries the collation instead. For a text column whose
effective collation is not byte-wise, the index key is the **ICU sort
key**, with the original string after a NUL:

```
key(v) = icu_sort_key(collation, v) ++ 0x00 ++ v.as_bytes()
```

`spg-storage` is `no_std` and holds no collator; it orders `IndexKey` by
a derived `Ord`. Handing it a sort key makes that byte comparison the
collation's comparison, with no change to the B-tree and no change to
the on-disk format. The trailing original is the deterministic
tiebreak, which `collate::compare` already applies for the same reason
(PG's locale collations have `collisdeterministic = t`).

Proven before building on it: over three locales and 29 strings
including accents, punctuation, case and canonically-equivalent forms,
comparing sort keys as bytes gives the same answer as `compare` on all
2,523 pairs (`sort_key_bytes_order_the_way_compare_does`).

A byte-wise column keeps `IndexKey::Text(v)` — byte-identical to today,
so no index that exists now changes.

**Landed 2026-08-23.** The keys go through the channel expression
indexes already use: the engine supplies them, `Table::add_index` leaves
a collated tree EMPTY because only the engine can encode ICU sort keys,
and `Table::index_on` declines an unfilled one. That decline lives in
one place rather than at each of the dozen seeks, because an empty tree
answers no rows to everything and that is indistinguishable from a
correct answer — ablating it turns seven pinned cases red, all with
empty results.

Seeking works for equality, `IN` and range bounds, all through the one
`probe_key` funnel so they cannot disagree with each other. The
top-N ORDER BY walk is still declined for a collated column: the tree
now walks in the locale's order so it could be allowed, and that is a
missed optimisation rather than a wrong answer.

### S1 — the database's collation is persisted, and set once

**Landed 2026-08-23.** `Catalog.db_collation`, FILE_VERSION 92, one tag
byte plus the name. Absent reads as `C`, so an image from any earlier
version loads unchanged — the v52-compatibility test caught the new byte
on its first run, which is what it is for.

`spg-server` reads `SPG_LC_COLLATE`, then `LC_ALL`, `LC_COLLATE`, `LANG`
— POSIX's precedence and `initdb`'s — and records it on a database that
does not have one. A database that does keeps it, silently; only a
MISMATCH says anything, and it says it loudly, because the server has
then been told one thing and the data on disk says another.

Recorded in the catalog at creation. Sourced as `initdb` sources it:
`LC_ALL`, then `LC_COLLATE`, then `LANG`; an explicit `CREATE DATABASE …
LC_COLLATE` overrides. **Absent on disk means `C`**, which is what every
database written by every earlier version was built with — so an upgrade
changes nothing, rebuilds nothing, and cannot be wrong.

Immutable afterwards, with PostgreSQL's own refusal. That is not a
limitation borrowed for convenience: it is what makes S0's index keys
sound. Every key in an index was built under the collation the database
was created with, and that collation cannot move out from under it.

### S2 — an undeclared text column inherits it

**Landed 2026-08-23**, at five comparators: the statement's own ORDER
BY, an aggregate's ORDER BY, `min`/`max`, the interpreted comparison,
and the compiled scan filter. The last of those was the one that needed
`TextCompare` to grow a third field — it carried *fold* and *pad* and
not *order*, so `WHERE x < 'b'` answered by bytes while `ORDER BY x`
over the same column answered by the locale, inside one query. That
type's own comment says it exists to stop the interpreted and compiled
paths disagreeing.

Two traps, both found by measurement rather than by reading:

- **`byte_wise` must not suppress it.** That flag is true whenever a
  column carries `Collation::Binary`, which is the struct's DEFAULT and
  means *nothing was said about folding*. Letting it zero the ordering
  made inheritance silently do nothing — the same misreading of the same
  default that made `column_key_is_bytewise` drop three rows.
- **The probe and the entries must agree on WHICH collation.** Storage
  keys an inheriting column's index under the database collation;
  `probe_key` read only the column's declared one. For one commit,
  `x = 'apple'` on an indexed inheriting column returned nothing at
  all.

At comparison, sort and index-key time — **not** stamped onto the
column, because PostgreSQL reports `collation_name` as NULL for an
inheriting column and stamping would make SPG report a name where the
oracle reports nothing.

S0 must land first. Inheriting a locale collation before index keys
carry one would spread the missing-rows defect from the handful of
columns that declare a collation today to every text column in the
database.

### S3 — refuse rather than guess

**Landed 2026-08-23**, and narrower than it sounds. ICU falls back to
the root collation for a well-formed language tag it has no data for, so
`zz_ZZ` is accepted — this build really can perform it, as root. What is
refused is a name that is not a language tag at all. PostgreSQL
validates against its own catalogue and answers `collation "x" for
encoding "UTF8" does not exist` for both kinds; SPG cannot tell them
apart, because the parser has no collator to ask. Recorded, not fixed.

If a build cannot perform the collation a database was created with,
opening it fails loudly. Comparing by bytes instead would be answering
with a different comparator than the indexes were built with, which is
the one thing this whole design exists to prevent.

## What this costs, and who pays

- **An existing SPG database**: nothing. No persisted collation means
  `C`, byte-wise keys, byte-identical behaviour. The entire existing
  test suite is the negative control for this claim.
- **A new database in an environment with a locale**: text indexes hold
  sort keys, which are larger than the strings they encode. That is the
  price of an index that agrees with the scan, and it is the price
  PostgreSQL pays too.
- **A deployer who wants byte order**: `LC_COLLATE=C`, one variable, the
  same one PostgreSQL reads.

## Why not the other two options

**Change the default and rebuild on upgrade.** Every index in every
existing database rebuilt at first open after an upgrade, on data whose
size we do not control, to change an answer the customer did not ask to
change. The upgrade is the wrong moment to reorder a database.

**Make it settable on a live database.** PostgreSQL does not allow it,
and the reason is the one above. Offering it would mean owning a
reindex-everything path whose failure mode is silently wrong answers.
