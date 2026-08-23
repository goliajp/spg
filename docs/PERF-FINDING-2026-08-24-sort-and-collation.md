# Two perf findings, and why the gate could not see either

Both came out of auditing the three customer shapes still behind
PostgreSQL 18. Neither is the thing the audit went looking for.

Every figure below is interleaved on one machine, min-of-3-in-session per
sample, medians across samples, both engines started in the same window.

## 1. A locale database collation cost 26x — shipped in v7.38.18

`WHERE kind = 'click'` over 200,000 rows:

| | |
|---|---|
| SPG, database collation `C` | 2.177–3.086 ms |
| SPG, database collation `en_US.UTF-8` | **58.292–76.242 ms** |
| PostgreSQL 18.4, `en_US.utf8` | 1.487–4.391 ms |

Two causes, both the same shape: work done per row that belongs once per
query.

**The fold.** `Step::BinaryCi` exists to fold case and padding, and was
emitted for every text comparison whenever a collation declared an
ORDER, because the guard asked `is_plain_bytes()`. Its `fold_one` then
copied each side into a fresh String and left it exactly as it found it.

**The collator.** `collate::compare(name, a, b)` parses the locale and
builds a whole ICU `Collator` on every call — once per row. The same
defect v7.38.18 fixed for sorting, where `Collated` was introduced for
exactly this; the scan-filter path was never connected to it.

Fixed in v7.38.19. Equality now skips the fold entirely, and it does so
for a reason rather than for speed: PostgreSQL's locale collations are
**deterministic**, so two strings that compare equal are byte-identical
and the collator cannot change the answer. Ordering keeps the collator,
resolved once at compile time.

| | before | after | PG 18.4 |
|---|---|---|---|
| text equality | 58.5 ms | **2.6 ms** | 1.94 |
| text ordering `<` | 62.5 ms | **15.3 ms** | 3.29 |
| the customer's dashboard shape | 16.6 ms | **10.8 ms** | 7.18 |

**Why no gate saw it:** all sixty-four cells of the constant-answer sweep
start their SPG leg under `C`. A locale-collation panel now runs beside
them, measured against the same binary under `C` rather than against
PostgreSQL — same box, same window, so the machine's speed cancels and
what is left is the question worth asking: does declaring a collation
change the cost class of an ordinary query.

## 2. Sorting is 1.2–2x behind, and the sweep's ORDER BY cells cannot see it

The sweep has eight ORDER BY shapes at four row counts and SPG wins all
of them. They are all of the form `SELECT pad FROM t ORDER BY k` — four
hundred thousand rows of TEXT come back over the wire, and **SPG's wire
encoding is fast enough to dominate the cell**. The sort is in there
somewhere, under the transfer.

Wrapping the same work in `count(*)` returns one row and isolates it.
200,000 rows, narrow table, collation `C`:

| 200,000 rows, ad-hoc narrow table | SPG | PG 18.4 | |
|---|---|---|---|
| `ORDER BY` a text column | 52.2 ms | 42.4 | 1.23x behind |
| `ORDER BY` an int column | 29.9 ms | 14.6 | 2.05x behind |

**The int figure does not generalise, and the panel below is what found
that out.** Run against the sweep's own fixture at 400,000 rows, the same
isolated int sort is a WIN:

| 400,000 rows, the sweep's fixture | SPG | PG 18.4 | |
|---|---|---|---|
| sort only, text | 197.8 ms | 81.6 | **2.43x behind** |
| sort only, int | 52.2 ms | 63.9 | 0.82x — SPG ahead |
| sort only, two keys | 129.2 ms | 79.1 | 1.63x behind |
| sort only, distinct text | 23.8 ms | 25.4 | 0.94x — SPG ahead |

So the reproducible gap is the **text** sort, not sorting in general, and
an attack aimed at "the int sort" would have been aimed at nothing. The
ad-hoc table that produced the 2.05x had no index and no statistics on
either engine; what it measured was that fixture.

The text figure is after this version's fix: `OrderKey::Text` held a
`String`, so ordering 200,000 rows made 200,000 allocations and 200,000
frees. A leaf-symbol profile put the allocator level with the work it
existed to do — `_xzm_free` and `_free` at 229 samples against
`_platform_memcmp`'s 205. `CompactText` stores fifteen bytes inline and
took it from 72.4 ms to 52.2.

**The text sort is the next decomposition**, at 2.43x on the sweep's own
fixture, and it starts from these numbers rather than from a fresh guess.
Two profiling attempts produced nothing usable — the first returned
almost no resolvable user-space symbols, the second spent its whole
window building the fixture — so the profile is still owed.

Second coverage gap, recorded here because it is the same disease as the
first: **the sweep never sorts a TEXT column.** Its shapes order by `k
INT`, `n NUMERIC` and `b BYTEA`. The text ordering path — the one this
version changed twice — has no cell at all.
