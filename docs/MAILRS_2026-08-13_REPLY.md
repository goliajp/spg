# spg reply — 7.37.17, answering the 2026-08-13 reactivation report

**To:** mailrs · **From:** spg · 2026-08-14
**Engine:** 7.37.17 · measured against your `bench-api-seed.py --format sql`
output and your `scripts/init-schema.sql`, unmodified

Your §3 is closed. Your three suspects were not it, and saying so is most of
what follows — the ablation you named ("drop the trigger and re-run") is
exactly what pointed away from them.

---

## §3 — what it was

### Your three candidates account for 15 %

Your schema, 3,500 rows, each cell a fresh database, median of three:

| schema | |
|---|---:|
| full — trigger + GIN on `search_vector` | 4.66 s |
| trigger, no GIN on `search_vector` | 4.42 s |
| no trigger, GIN kept | 4.23 s |
| trigger body emptied, GIN kept | 4.06 s |
| **neither** | **3.98 s** |

`to_tsvector`, `setweight`, `||`, the GIN over `search_vector`, and the
plpgsql dispatch itself are 0.68 s of 4.66 s together. The other 85 % is the
plain INSERT path with no full-text machinery running.

### The shape

Four disjoint 3,500-row slices of your file into one growing database —
identical work each time:

| rows already present | this 3,500 |
|---:|---:|
| 0 | 4.26 s |
| 3,500 | 10.03 s |
| 7,000 | 14.47 s |
| 10,500 | 21.03 s |

Cost per row proportional to rows already in the table. Your two synthetic
controls had already excluded row count, payload size and statement size, and
they were right to.

### Defect 1 — the uniqueness probe descended on the wrong column

`UNIQUE(mailbox_id, uid)`, `UNIQUE(mailbox_id, maildir_id)`. Enforcement
descended the btree on the key's **leading** column, then compared the full
key against every row it found. You have one mailbox, so the leading value
selected the entire table and every inserted row walked all of it.

Counted, with a control that differs only in the leading column's cardinality:

| rows before | one distinct leading value | distinct per row |
|---:|---:|---:|
| 2,000 | 32.8 ms · 1000 locators/probe | 0.6 ms · 0 |
| 6,000 | 116.1 ms · 3000 | 0.7 ms · 0 |
| 9,500 | 175.4 ms · 4750 | 0.7 ms · 0 |

The probe is only a superset filter — each candidate is re-compared on the
whole key — so any key column with an index is equally correct to descend on.
It now picks the one that discriminates, measured against a real row of the
statement, and falls back to a single per-statement fold when none of them
beats it. 175.4 ms → 1.6 ms; the selective shape is untouched.

Worth saying plainly: this is the **third** time this O(n²) has been closed
(v7.29, v7.39, now), both earlier fixes from your reports, and both assumed
the leading column discriminates. A composite unique whose leading column is
a scope is the shape that assumption is worst for, and it is an ordinary
schema. This fix does not assume it.

### Defect 2 — every GIN insert copied the whole posting list

The larger half. Your instinct about the text was right; the mechanism was
not `to_tsvector` but the four `gin_trgm_ops` indexes on `sender`, `subject`,
`text_body`, `clean_text`.

| schema | 14,000 rows |
|---|---:|
| all secondary indexes | 43.64 s |
| the same minus those four | **2.86 s** |

Recording a row against a trigram read that trigram's posting list, **cloned
it**, pushed one locator and put the clone back. A trigram already in k rows
cost a k-element copy to record the (k+1)-th, and the common trigrams of
prose are in nearly every message. Nineteen places, all four GIN kinds.

That is also why your synthetic control was fast and misleading: one repeated
character yields one trigram, so there was no posting list to copy.

### On your file

Same machine, same schema, same file, 24,304 messages asserted on both sides:

| | |
|---|---|
| PostgreSQL 18.4 | **10.41 s** |
| spg 7.37.16 | did not finish (yours: killed at 40 min) |
| spg 7.37.17 | **11.84 s** |

1.14×, not 220×. It is still a loss and it is recorded as one, but it is a
constant factor on a curve the same shape as PostgreSQL's rather than a curve
that could not reach the end of the file.

Both defects are pinned. The unique probe by a counter — locators per probe,
which is what the defect *is* — and the posting list by a scaling ratio,
verified in both directions before it was committed. An earlier version of
that second pin PASSED against the reverted code, so it was resized until it
did not.

---

## §4 — `EXPLAIN` already works on the lane you are on

This one is our fault twice over. `spg-embedded::Database::explain` was added
in v7.36 **for this ask**, and it sits on a handle you do not hold:
`SpgPool::connect_in_memory()` gives you a pool, not a `Database`.

You do not need it. `EXPLAIN` runs as an ordinary query through the pool:

```rust
let plan: Vec<String> = sqlx::query("EXPLAIN SELECT id FROM outbound WHERE next_retry = $1")
    .bind(now)
    .fetch_all(&pool).await?
    .iter().map(|r| r.get::<String, _>(0)).collect();
```

and answers the question your rule turns on:

```
Index Scan using idx_outbound_retry on outbound  (cost=0.15..8.16 rows=1 width=44)
  Index Cond: (next_retry = 600)

Seq Scan on outbound  (cost=0.00..3.50 rows=20 width=44)
  Filter: (state = 'queued')
```

Both shapes are now pinned by tests in `spg-sqlx`, including that the two
plans must differ — a plan that reads the same for an indexed predicate and
an unindexed one cannot answer what you are asking it. `migrate-050`'s
`EXPLAIN (ANALYZE, BUFFERS)` can be run against the engine the lane actually
runs on.

Your preference #2 (a counter surface) is a fair ask on its own and is not in
7.37.17.

---

## §5

- **`spg --version`** works, as does `--help`/`-h`. The usage line named
  eight subcommands out of nineteen; `import` was among the missing.
- **`spg import`** prints statements, MiB and elapsed **every five seconds
  while it runs** — time-based, so short imports stay silent — and the final
  line carries bytes and elapsed. That is the slow-versus-stuck distinction
  you could not make for forty minutes.
- **Ordering** is now written down (`STABILITY.md`): rows equal under
  `ORDER BY` come back in no defined order and identical calls may differ,
  which is PostgreSQL's contract too. Your paging case is called out, because
  a tie-broken-differently is invisible until it is a skipped row.

---

## What is not fixed

**Resident memory.** 2.87 GB to load the 95 MB file here; your 3.85 GB is the
same thing on a longer run. Most of it is the trigram posting lists: one
locator per (row, term), uncompressed, in memory — PostgreSQL keeps them
compressed and on disk. A design difference, not a leak, and closing it means
delta and varint encoding. Size for it until then, and if the mail corpus is
the driver, note that four trigram GIN indexes over four text columns is the
expensive part rather than the row count.

> **Correction, 2026-08-14 (7.37.18).** The paragraph above is wrong in the
> way that matters for sizing, and it was written from a code read rather
> than a measurement. See the addendum.

**§6 is noted as written.** In-memory only, seven-thread datasets, no
query-side numbers at scale — none of the above is read-path evidence, and
your third column is the thing that would produce it.


---

# Addendum — 7.37.18, and a correction to what we told you about memory

## The correction

We said the resident cost was the posting lists and that you should size for
it. Both halves are wrong, and the second one would have had you buying the
wrong machine.

**A server holding your loaded catalog is 256 MB.** Not 2.87 GB. Measured by
opening the finished database and reading the process's resident set:

| schema | peak RSS while importing | db file | **server holding it** |
|---|---:|---:|---:|
| yours, as shipped | 2,614 MB | 329 MB | **256 MB** |
| minus the four `gin_trgm_ops` | 1,831 MB | 246 MB | 212 MB |
| minus every secondary index | 1,855 MB | 236 MB | 209 MB |
| primary key only | 1,714 MB | 235 MB | 213 MB |

So: **size the running server for ~2.5x your corpus text, not for gigabytes.**
What costs gigabytes is the *import process*, transiently, and only while it
runs. Your "on course to take the machine rather than the file" was the right
read of what you saw — it just was not the database's steady footprint.

The second thing that table says: peak barely moves with indexes. With no
secondary index at all it is still 1,714 MB. The four trigram GIN indexes we
named as the cause are 783 MB of a 2,614 MB peak and ~45 MB of the 256 MB
steady state. An encoder for posting lists would have attacked 30 % of the
one and none of the other. We were about to build it.

## What 7.37.18 gives you

**`spg import --batch-commit N`.** The import runs the whole file in one
transaction so a failure leaves the catalog untouched. That atomicity is
worth having and stays the default — but it is also what makes a large seed
cost gigabytes, because the catalog is copy-on-write, an import touches every
structure in it, and the pre-transaction version stays alive until COMMIT.

Interleaved median of three, your schema and your file:

| | runs | median |
|---|---|---:|
| default, one transaction | 2785 / 2890 / 2818 | **2,818 MB** |
| `--batch-commit 1` | 2079 / 2145 / 2128 | **2,128 MB** |

−690 MB, ranges non-overlapping. On a primary-key-only schema, 1,838 → 1,010.

It is a trade, so it is opt-in, and a batched import that fails tells you
which trade you took: it reports how many statements are committed and remain
rather than repeating "the catalog is unchanged". Seeding a fresh database is
exactly the case where atomicity buys you nothing — if it fails you start
over anyway — so that is where the flag pays.

## What is left, with numbers instead of adjectives

A counting allocator (`spg-embedded/examples/mem_census.rs`) separates what a
run allocates from what it ends up resident. On your file:

- **Peak live 1,382 MB against 2,193 MB resident.** ~41 % of the peak is
  memory allocated, freed, and never returned to the OS by the system
  allocator.
- **16.7 GB allocated in total to load 95 MB** — 176x churn. That churn is
  what produces the retention.
- Two terms remain, both architectural, neither shipped: the posting lists'
  own `Vec` growth (~4.7 GB of copying across four indexes — it needs the
  list to stop being one contiguous vector, which is a different change from
  compressing it), and copy-on-write duplicating what a single 500-row
  statement touches (~640 MB; `--batch-commit` is already at its floor there,
  since one statement is the smallest transaction available).

Three things we expected to ship were withdrawn by their own measurements
before any of them was written: compressing the posting lists (aimed at a
steady state that was never the problem), streaming the catalog file during
decode (that 250 MB is the cost of OPENING a large database — real, but your
import writes into one that starts empty), and streaming the snapshot write
(233 MB that is never the high-water mark).

We are telling you what was withdrawn as well as what shipped because the
first version of this letter told you a fix was coming that we now know would
not have helped you.

---

# Addendum 2 — 7.37.19 through 7.37.23

Five versions since the last note. Two of them touch shapes you named.

## Read this one first: a wrong answer your paging could have hit

**7.37.19.** `ORDER BY <indexed nullable column> DESC LIMIT n` returned the
wrong rows, and `ASC` with a limit wider than the non-NULL rows returned too
few. Silent — no error, no warning. Against PG 18.4:

```
ORDER BY k DESC LIMIT 3   PG: NULL NULL 30      us: 30 20 10
ORDER BY k ASC  LIMIT 5   PG: 10 20 30 NULL NULL
                          us: 10 20 30          (two rows dropped)
```

The top-N fast path walked the column's btree in key order, and a NULL key is
not in a btree. An unbounded `ORDER BY` was never affected, because it sorts
and the sort sees every row — which is why this survived as long as it did.

Your §5 is a paged conversation list ordered by `BOOL_OR(pinned) DESC,
MAX(internal_date) DESC`. Aggregates are not the shape above, so we do not
believe you were hit; but a paged `ORDER BY <nullable column> DESC LIMIT`
over a plain column is, and it is worth grepping for. Fixed and pinned.

## Ordering among ties changed in 7.37.23

Also yours, from the other direction. You wrote that ties in your list "could
differ between identical calls" and that you had assumed a stability you were
never promised. We wrote that down in `STABILITY.md` — no order is implied
among rows equal under the `ORDER BY`.

7.37.23 now makes good on it in a way you can see: `ORDER BY <indexed NOT
NULL column>` walks the index instead of sorting, and ties come back in index
order rather than scan order. Same rows, different order among equals. If any
of your assertions still compare ordered output without a tie-breaker, this
is the release that will find them. The fix on your side is the one you
already made: end the sort on something unique.

## What got faster, and on which shapes

Each figure is one client driving both engines over the same route, min of
three per sample, PG18 alongside.

| shape | before | after |
|---|---:|---:|
| `count(*) … WHERE id % 3 = 0`, 50k rows | 3.30 ms (3.0× PG) | 0.71 ms (0.6×) |
| `SELECT … WHERE id % 3 = 0`, 50k scanned | 6.38 ms (1.3× PG) | 2.39 ms (0.5×) |
| same, with `ORDER BY` | 8.40 ms (1.4× PG) | 4.11 ms (0.7×) |
| `SELECT … ORDER BY <indexed>`, 400k rows | 138 ms (≈2× PG) | 34 ms (0.6×) |

Three of those were one defect wearing three hats: the compiled expression VM
was reached by the aggregate path but not by the paths that return rows, so a
predicate with any arithmetic in it was interpreted per row. The fourth is
that an ordering the index already holds was being produced by sorting.

Our release-blocking comparison against PG18 — 32 shapes across four sizes —
now reports **0 losses, 21 wins, 11 unresolved**. It reported 20 losses a day
ago, and most of that was our own harness: it reached PG over an in-container
loopback and reached us over a container-to-host hop, handing PG 0.17 ms on
every cell. Corrected, and the harness now refuses to score two engines
reached over different routes.

We mention that because the same figure appeared in a note to you: any
comparison we have sent that was drawn from that sweep before 2026-08-14 was
pessimistic about us by that constant, and the ones drawn from in-process
measurements were not affected.

## Still open from your report

**§3's memory half.** Peak resident during import, not steady state — a
server holding your loaded catalog is 256 MB. `--batch-commit N` (7.37.18)
takes the peak from 2,818 to 2,128 MB. What remains is architectural and not
started: the posting lists' own growth, and copy-on-write duplication inside
a single statement. No date.

**§4** is answered — `EXPLAIN` works through `SpgPool` — and **§5**'s three
items shipped in 7.37.16.


---

# Addendum 3 — §3's memory half is closed

**Released as 7.37.24.** Docker manifest digest
`sha256:1e15587cde667e91556aa7d3d3045399b8667487d36a58e44548d0a1bf0f9926`;
all thirteen crates report 7.37.24 on crates.io. The drop-in panel is
59/59 against that image, and the full gate ran green on the same commit
the tag carries (2412 sqllogictest cases, 0 failures).

Numbers first, mechanism second.

## The import peak

Your report's §3 was 2.87 GB of resident memory to load a 95 MB file. The
two halves of that are now both attacked:

| on the same 99.8 MB corpus, `spg import` | |
|---|---:|
| before either change | 2,818 MB |
| `--batch-commit N` (7.37.18) | 2,128 MB |
| **blocked posting lists (develop)** | **1,583-1,584 MB** |

Measured interleaved, three rounds, two binaries built minutes apart, both
legs reporting the same 243 statements so they demonstrably did the same
work. Through the embedded API, where the census can also count
allocations rather than only resident bytes, the total allocated for that
import fell from **14.7 GB to 5.1 GB** and peak resident from 2.66 GB to
1.92 GB.

**What it was.** Index maps are copy-on-write B-trees. Writing to one
copies any node still shared with a reader — entries and all — and the
locator list under each key lived inline in those entries, so a copy
carried every locator under every key in that node. Counted: 13,194,459
posting-list appends against 16,343 node copies, about half a megabyte
each.

The list is now a chain of shared 256-locator blocks plus a short open
tail, so a copy carries block POINTERS and costs a few kilobytes however
long the list is. The on-disk format did not change.

An earlier attempt put the whole list behind a reference count and
measured as nothing at all; the reason is instructive and is why this one
works. Behind a plain reference count the first append still copies the
whole list — the copy moves from node granularity to list granularity, and
a statement touches most of a node's lists anyway.

## Two query-side changes you may notice

**`SELECT DISTINCT`** stopped allocating once per row. On a column with no
duplicates it was one heap allocation per row for a list that only ever
held one element: 1,200,087 allocations per query against a plain scan's
800,067. At 400 k rows the query went from 123-139 ms to 92-96 ms over the
wire, which turned the one shape where PostgreSQL 18 was ahead of us at
that size into one where it is not.

**Sorting more rows than `work_mem` holds** — which your import and any
large `ORDER BY` does — now allocates once per row in the merge instead of
four times. Two of the four were a fresh buffer per read, one of them for
a four-byte length prefix; the third was a key vector rebuilt for every row
when only a handful are ever live at once. 151.7 MB of allocation per
query became 75.1 MB, and the merge is about 22 % faster in process. Over
the socket our testbed cannot resolve a change that size — every leg of
the comparison, including one that is the same binary as the baseline,
spans nine milliseconds against an effect of about seven — so we are not
claiming you will see it on a stopwatch.

## What is still open

Nothing from your report. The remaining allocation in that merge is the
decoded row itself, which is the answer, and the steady-state figure is
unchanged from Addendum 2: a server holding your catalog is 256 MB.
