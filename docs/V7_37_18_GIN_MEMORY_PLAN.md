# v7.37.18 — the memory half of mailrs's report

> **Phase A has run. It inverted this document's target — see §A-RESULTS
> before Phase B. The posting-list encoder this was named for is not the
> lever, and steady-state memory was never the problem.**

**Target:** loading their 95 MB seed costs 2.87 GB resident. Bring that down
far enough that a full-text mail schema sizes like a database rather than
like a memory cache.

**Status of the premise:** NOT established. See Phase A before reading Phase B.

---

## 0. Why this does not start with the encoding

The obvious plan — compress GIN posting lists with delta + varint, as
PostgreSQL does — may well be right. It is not yet earned:

- The only evidence that posting lists dominate is **one ablation at 14,000
  rows**: 787 MB with the four trigram GIN indexes, 322 MB without, so 59 %.
- The full file is 24,304 messages and 2,871 MB. Scaling the 14k figure by
  rows predicts ~1,370 MB. **The measured number is more than twice that.**
  Something grows faster than rows, or something else is holding memory, and
  neither is identified.

Attacking a target whose share has not been measured is the exact shape that
cost the kevy v1.29 train three throughput-neutral attacks: userspace memcpy
was genuinely 16 % of cycles, eliminating it moved throughput by nothing,
because 16 % of cycles is a tax and not the bottleneck. The same trap exists
for memory. Phase A is the pre-Phase-B gate.

---

## A-RESULTS — what Phase A measured (2026-08-14)

Every figure below is the full 95 MB file, 54,941 rows, on the same machine.

### The headline: the resident cost is transient, and it is not the indexes

| schema | peak RSS (import) | db file | **server holding it** |
|---|---:|---:|---:|
| full, as mailrs ships it | 2,614 MB | 329 MB | **256 MB** |
| minus the 4 `gin_trgm_ops` | 1,831 MB | 246 MB | 212 MB |
| minus every secondary index | 1,855 MB | 236 MB | 209 MB |
| primary key only | **1,714 MB** | 235 MB | **213 MB** |

Two things fall out and both contradict the premise this document was
written on:

1. **Steady state is 209-256 MB, not 2.87 GB.** A server holding the loaded
   catalog is roughly 2.5x the input text, which is an ordinary number. What
   costs gigabytes is the *import process*, transiently.
2. **Peak barely moves with indexes.** With no secondary index at all it is
   still 1,714 MB. The four trigram GIN indexes are 783 MB of a 2,614 MB
   peak — real, but not the majority, and none of the steady state.

An encoder for posting lists would therefore attack ~30 % of the peak and
0 % of what an operator's machine holds afterwards.

### Where the transient peak actually goes

| lever | measured |
|---|---:|
| the single wrapping transaction | **~890 MB** |
| loading the catalog file | ~250 MB (file buffer held through decode) |
| serialising it back out | 138 MB |
| the script text itself | 95 MB |
| the catalog, in memory, for real | 213 MB |

- **Wrapping transaction.** `import_script` runs the whole file inside one
  `BEGIN`/`COMMIT` unless the script owns its own transaction. Forcing
  per-statement commits (a `BEGIN; COMMIT;` at the head of the file makes
  `script_owns_tx` true) drops the peak from 1,873 to 984 MB on the PK-only
  schema, and from 2,855 to 2,095 MB on the full one. This is the largest
  single lever by a wide margin, and it is the copy-on-write catalog keeping
  the pre-transaction version alive while a transaction that touches every
  structure stays open.
- **Load, not save.** Opening the finished 250 MB catalog and running
  `SELECT 1` peaks at 625 MB; adding a `CHECKPOINT` takes it to 763 MB. So
  reading and decoding costs ~410 MB over the 213 MB it produces, while
  serialising the whole thing back out costs 138 MB. The guess this document
  was about to act on — that the save path holds two full copies plus `Vec`
  doubling — is **wrong**, and it was a code read rather than a measurement.
- **Chunked import confirms the shape.** Ten ~10 MB chunks, each its own
  process, peak at 83, 138, 178, 283, 354, 412, 473, 532, 679, 1,128 MB as
  the catalog grows. Each process holds only its own 10 MB of script, so the
  peak tracks the catalog it must load, not the SQL it must run.

### Where the accounting still does not close

984 MB (PK-only, per-statement commits) against 213 MB of real catalog plus
95 MB of script leaves ~670 MB, and the load-path measurement above accounts
for roughly 410 MB of it. The remaining ~260 MB is not yet named. **A3 and A4
still owe an absolute per-structure accounting**; the ±20 % rule stands, and
this document does not authorise Phase B until it closes.

What A5 already settled is that the question changed: this is about the
import path's transient memory, not about how the database stores anything.

---

## Phase A — make the memory account balance (read-only, no product change)

**Exit criterion, hard:** a per-structure accounting that reconciles to
**±20 % of measured peak RSS** on the full file. Below that, Phase B does not
start. Above 20 % unaccounted, the missing piece IS the finding.

### A1 — ablation by index class at FULL size

Four runs of the real file, peak RSS each (`/usr/bin/time -l`):

| schema | what it isolates |
|---|---|
| full (as shipped by mailrs) | the number to explain |
| minus the 4 `gin_trgm_ops` indexes | trigram postings |
| minus every secondary index | all index maintenance |
| primary key only | rows + PK |

The 14k-row version of this already exists; this is the same experiment at
the size where the discrepancy appeared. Cheap: ~12 s a run.

### A2 — row storage amplification, on its own

Load with no indexes at all and compare peak RSS against the 95 MB input.
A `Value::Text` per cell carries a `String` (ptr/len/cap plus the allocation),
and the dense row codec stores nulls as zero bytes but hot rows live decoded.
If rows alone cost 500 MB+ of the 2.87 GB, the plan has a second target and
the title of this document is wrong.

### A3 — absolute accounting, not just differences

Add a diagnostic that walks the catalog and reports heap bytes by category:
row values, btree index maps, GIN maps split into keys vs posting lists,
cold-segment metadata, everything else. An `spg-engine` example under
`--features perf-counters`, in the shape of `probe_uniq_composite.rs`.

Ablation gives differences; this gives absolutes. **They must agree.** Where
they disagree, one of them is wrong and that is worth knowing before writing
an encoder.

### A4 — the census that checks the model

Per trigram index: number of distinct keys, total posting entries, and
`size_of::<RowLocator>()` asserted rather than assumed (the enum is
`Hot(usize)` | `Cold { u32, u32 }`, so it should be 16 bytes, and "should be"
is not a measurement).

Predicted bytes = entries × size_of. Compare against A3's measured category.
A mismatch means `Vec` capacity slack, allocator overhead, or B-tree node
overhead is a term I have not modelled — all three are plausible and one of
them may be the actual story. Capacity slack in particular: a `Vec` that grew
by doubling sits at up to 2× its length, and every posting list grew by
repeated push.

**That last point may be the whole 2× discrepancy.** If so, the fix is
`shrink_to_fit` at quiesce, not an encoder, and it is a day rather than a
release. Phase A finds out.

### A5 — is it retained or transient?

Peak RSS is not the same as steady-state. Measure resident memory *after* the
import completes and after a checkpoint, not only its peak. If peak is
transient (encode buffers, the persistent map's path-copying under a
snapshot), the operator-visible problem is different from the one an encoder
solves.

---

## Phase B — REORDERED BY MEASUREMENT (2026-08-14)

The original Phase B, an encoder for posting lists, is kept at the bottom
because that is where the measurement puts it. Land in this order, measuring
after each, and stop as soon as the number is acceptable — every step below
is cheaper and less invasive than the one after it.

### B1 — bound the import's transaction — DONE, measured −690 MB

`spg import` wraps the whole file in one transaction so a failing statement
leaves the catalog untouched. That atomicity is a real feature — it is what
prints "import rolled back — the catalog is unchanged" — and it is also what
keeps the pre-transaction catalog alive for the length of a 95 MB seed.

Do not silently drop it. Add `--batch-commit N`, defaulting to **off**
(today's atomic behaviour), which commits every N statements. An operator
seeding a fresh database trades atomicity they do not need for a bounded
peak; everyone else is byte-for-byte unchanged.

Then say so where it will be read: the `spg import` usage line, and the
progress line r1018 added, which is exactly where someone watching a large
seed is looking.

**Result (r1019).** `--batch-commit N` shipped, default off. Interleaved
median of three on mailrs's full schema and file:

| | runs | median |
|---|---|---:|
| default, one transaction | 2785 / 2890 / 2818 | **2818 MB** |
| `--batch-commit 1` | 2079 / 2145 / 2128 | **2128 MB** |

−690 MB, −24 %, non-overlapping ranges. On the PK-only schema it is
1838 → 1010 MB, which corroborates the 1873 → 984 measured before the flag
existed.

**It does not reach the target on its own.** 2,128 MB against a goal of under
1,000. B2 is required, and what is left after B1 is the catalog load path and
the index structures rather than the transaction. Intermediate batch sizes
(10, 25) were measured once each, disagreed with each other, and are not
reported — one run is not a reading.

Two pins, both semantic rather than timing: the default stays all-or-nothing,
and a batched failure keeps its committed batches AND stops claiming "the
catalog is unchanged" — the sentence an operator decides whether to re-run
the file on.

### B2 — WITHDRAWN: it was aimed at the wrong path

Opening a 250 MB catalog does peak at 625 MB to produce 213 MB of structures,
so the load path is genuinely heavy — for a server starting on a large
database. It is not mailrs's cost: their import writes into a database that
starts empty, so there is no 250 MB file to read at open. The number came
from a `SELECT 1` experiment, which measured opening, and was then written
into a plan about importing.

Kept as a real finding for whoever works on server start-up. Not part of this
version.

### B3 — the ~700 MB, named (Phase A3)

A counting global allocator (`spg-embedded/examples/mem_census.rs`) separates
what a run ALLOCATES from what it ends up resident. Both numbers, same run,
PK-only schema with `--batch-commit 1`:

| | |
|---|---:|
| peak live (allocator high-water) | **582.6 MB** |
| peak RSS | **994 MB** |
| **gap — allocator retention** | **~411 MB (41 %)** |
| total ever allocated | **7,590 MB** |
| live catalog after the script is dropped | 326 MB |
| snapshot buffer | 140 MB |

So 41 % of the peak is memory the process allocated, freed, and the system
allocator never returned. It is driven by churn: **7.6 GB allocated to load a
95 MB file, 80x**. On mailrs's full schema it is **16.7 GB, 176x**, peak live
1,382 MB against 2,193 MB resident.

Isolating the four trigram GIN indexes (full schema, minus only those four):

| | full | minus 4 trgm GIN | delta |
|---|---:|---:|---:|
| ever allocated | 16,699 MB | 7,454 MB | **−9,245 MB** |
| peak live | 1,382 MB | 585 MB | −797 MB |
| peak RSS | 2,193 MB | 1,149 MB | −1,044 MB |
| live catalog | 675 MB | 384 MB | −291 MB |

The trigram path is the dominant term — but as **churn during maintenance**,
not as stored size, which is not what this document originally claimed.

### B3a — trigrams off the heap: measured, and it was not the term

`extract_trigrams` returned `BTreeSet<String>` and allocated one `String` per
WINDOW, before deduplication: ~3,400 heap allocations per 3.4 KB body per
index, four indexes, 54,941 rows. Hundreds of millions of three-byte strings
in twenty-four-byte headers. It looked like the obvious cause of 9.2 GB.

It is not. Trigrams are now `[u8; 3]` on the stack, the `String`-keyed maps
are addressed through a new `Borrow`-generic `get_by`/`get_mut_by`, and a
`String` is allocated only for a key the map has never seen:

| | before | after |
|---|---:|---:|
| ever allocated | 16,699 MB | **15,219 MB** (−9 %) |
| peak live | 1,382 MB | **1,411 MB** (no change) |

Peak live comes from the allocator counter rather than from RSS, so "no
change" is a fact and not a noise band. **The third guess in this document to
be refuted by its own measurement.**

By arithmetic the remaining term is the posting lists' own growth: ~36 M
entries per index at 16 bytes is 583 MB, and a `Vec` that doubles copies
about twice that over its life — ~4.7 GB across four indexes. That is not
removable by allocating less; it needs the list to stop being one contiguous
`Vec`, which is B5's blocked representation arriving from a different
direction than compression.

The change is kept: −1.5 GB of churn and hundreds of millions of allocations
are worth having, and `get_by`/`get_mut_by` is the lookup a blocked
representation will need. It is recorded as hygiene, not as the fix.

### B3b — the 612 MB inside one statement, named (r1026)

Peak live 1,382 MB against 770 live at COMMIT: ~612 MB is transient within a
single 500-row statement, even with `--batch-commit 1`, where that statement
IS the transaction.

**Who holds the second reference.** A transaction keeps its own handle on the
catalog (`state.catalog`) so it can roll back, while the engine keeps
`self.catalog`. Two handles means `Arc::make_mut` finds the spine shared, so
every node the statement touches is copied. That much is the price of
transactional rollback on a copy-on-write catalog, and it is not a defect.

**What is a defect is what a node copy copies.** `BNode::clone` does
`children.clone()` — a `Vec<Arc<_>>`, so pointers — and `entries.clone()`,
a `Vec<(K, V)>`, which clones the VALUES. The GIN and btree maps store
`Vec<RowLocator>` posting lists directly as `V`. So copying one node
deep-copies **every posting list it holds**, including the ones the statement
is not touching.

Counted rather than argued (temporary counters in the `Clone` impl, removed
after the reading):

| | over the whole import | stmt 40 → 60 |
|---|---:|---:|
| node clones | 16,343 | 3,697 |
| entries (posting lists) copied | 70,468 | 15,967 |

Peak live rose 727.9 → 1,415.3 MB across that same window, +687 MB. Spread
over 15,967 copied lists at 16 bytes a locator, that is ~2,690 locators per
list — which is the right order for a table holding 20-30k rows at that
point, where common trigrams are in most of them.

**The fix that was written, and did nothing (r1026).** Make the value cheap
to clone:
`Arc<Vec<RowLocator>>` instead of `Vec<RowLocator>`. A node copy then moves
pointers, and a posting list is cloned only when the statement actually
appends to that list — once, after which the handle is unique and the rest of
the statement mutates in place. With eight entries a node and typically one
or two of them being written, most of those 70,468 copies are collateral.

Blast radius: the four GIN kinds and `BTree`, their maintenance sites, the
three lookup methods, and the catalog codec — about forty sites. It was
written, the workspace compiled, and the lookups kept handing out
`&[RowLocator]` so nothing outside the crate changed shape.

**It moved nothing.** Peak live 1,415.3 → 1,413.6 MB, total allocated
15,219 → 15,234 MB, peak RSS 2,406 → 2,408. Reverted.

The null result is the useful part. Had those 70,468 copied entries been
posting lists averaging the ~2,690 locators the arithmetic suggested, the
change would have removed about 3 GB from the 15.2 GB total. Nothing moved,
so **the copied entries are mostly SHORT lists** and the arithmetic fit was a
coincidence.

And the mechanism explains itself in hindsight: `Arc::make_mut` clones a
shared list too. The change moved the copy from "when the node is copied" to
"when the list is first written", it did not remove it — and the lists that
get copied are the ones being WRITTEN, not innocent neighbours sharing a
node. There is no collateral to save.

**Which points at the other item.** The copying that remains is a
2,690-element list being cloned to append one locator. That is not fixable by
making the value cheap to clone; it needs the list to stop being one
contiguous `Vec`, so an append touches a tail block instead of the whole
list. B5's blocked representation, arriving from the memory side rather than
the churn side.

**The fix, not yet written.** Make the value cheap to clone:

### B4 — WITHDRAWN: it does not touch the peak

The snapshot buffer is real — 233.5 MB live on mailrs's schema — but it is
built at the END, and peak live by then is 1,411 MB against the 931 MB the
snapshot pushes live to. Streaming it would remove 233 MB of memory that is
never the high-water mark.

Measured before it was implemented, which is the only reason it is not in
this version by mistake.

### B5 — posting-list representation (was the whole of this document)

The four trigram GIN indexes are 783 MB of the full schema's 2,614 MB peak
and ~45 MB of its 256 MB steady state. If B1-B4 bring the peak under the
target, this is not needed for mailrs's report at all, and it should then be
justified on its own terms (snapshot size, cold-start time) rather than on
theirs.

If it is still wanted, the original staging holds and is still the right
shape: change the lookup API to an iterator while the representation stays a
`Vec<RowLocator>` (zero behaviour change, provable by the gate alone), then
narrow the element to `u32` row indices, and only then encode. The detail is
preserved below under "B5 detail (original Phase B)".

### B5 detail (original Phase B)

### B5.0 — decide the representation against the read path

Two candidates:

| | delta + varint (PostgreSQL's) | roaring bitmap |
|---|---|---|
| bytes/entry, dense | ~1.1 | ~0.1–2 |
| sequential scan | fast | fast |
| intersection of many lists | decode both sides | native, much faster |
| code | ~200 lines, no dependency | a dependency, or ~1000 lines |
| fits `no_std` + `alloc` | yes | yes, with work |

Trigram search intersects one list per trigram of the pattern, so
intersection cost is the read-side question, and it is a question with an
answer: bench `LIKE '%…%'` against a corpus-sized index under both. Do not
pick on taste.

Delta + varint is the default because it is small and self-contained; a
periodic skip index (every 128th entry, absolute) keeps intersection from
degenerating to full decode.

### B5.1 — change the API shape first, keeping the representation

`Index::gin_lookup_word`, `gin_trgm_lookup`, `gin_jsonb_lookup` all return
`&[RowLocator]`, which a compressed list cannot produce. Turn them into
iterators **while still backed by `Vec<RowLocator>`**.

Zero behaviour change, so the full gate proves the refactor on its own. Every
caller is then already shaped for a compressed backing, and the step that
actually changes representation touches storage internals only.

### B5.2 — narrow the element

Hot locators are row indices. `Vec<u32>` for the hot postings plus a rare
side list for cold ones is 16 → 4 bytes with no encoder at all. Land and
measure this before B3: if it takes 2.87 GB to something acceptable, B3 may
not be needed, and 4× for a mechanical change is a good trade against the
complexity of an encoder.

### B5.3 — encode

Delta + varint + skip index, behind the B1 iterator. Append becomes
"append to the tail block"; the structure stays append-friendly because
posting lists are built in ascending row order — which is exactly the
property r1018's in-place append relies on, so it is already true.

Deletes and vacuum rewrite blocks rather than removing an element in place.
Cost is bounded by block size; measure it against the autovacuum gate.

### B5.4 — the on-disk form

Posting lists are persisted (catalog tags 3/4/5/6, FILE_VERSION 21/24+).
Keeping the disk format as it is means decode-on-load and encode-on-save,
which works and costs nothing at steady state. Compressing on disk too is a
FILE_VERSION bump and a separate decision — snapshot size is not what mailrs
reported, so it is out of scope unless A says otherwise.

---

## Phase C — what pins it

- **A memory ceiling test** over a corpus of the reported shape: N rows of
  prose, four trigram GIN indexes, assert steady-state bytes per row under a
  bound. Ceiling, not ratio — the defect is an absolute cost.
- **A read-side perf gate** on trigram search, so compression cannot buy
  memory with query latency. Must exist BEFORE B3 lands, or the regression it
  is meant to catch has nowhere to be caught.
- **Both verified in both directions.** The r1018 GIN pin passed against the
  reverted code on its first version; a pin that has not been shown to fail
  is not a pin.
- **Compat**: `dump_compat` and `data_compat` already load fixtures carrying
  GIN indexes. They must stay green through B1-B3 without regenerating
  fixtures — regenerating them would hide exactly the breakage they exist to
  catch.

---

## Blast radius

`spg-storage`: the four GIN kinds, three lookup methods, the maintenance
sites r1018 touched, the catalog codec's read/write path, vacuum. Stone-layer
by the project's own classification, so: full gate per step, differential
against PG for any query-visible behaviour, and the steps land separately.

Callers outside storage go through the three lookup methods and
`try_gin_lookup` / `try_trgm_seek` in the engine — a small, enumerable set,
which is why B1 is cheap.

---

## Stop conditions

Rewritten after Phase A, which tripped two of the originals.

- **The account still does not close to ±20 %.** ~260 MB of the PK-only peak
  is unnamed. B1 and B2 are justified by their own measurements and may
  proceed; B3 may not, and anything past B4 waits on A3/A4.
- **B1 alone reaches the target.** Then stop and say so. `--batch-commit` is
  ~890 MB of a ~1,700 MB floor.
- **A guess is standing in for a measurement.** Twice now in this document:
  posting lists were assumed to dominate (they are 30 % of peak and none of
  steady state), and the save path was assumed to hold two full copies plus
  doubling (it costs 138 MB). Both were code reads. Neither survived an
  hour of measurement.
- **Steady state was never the problem.** 209-256 MB for a 95 MB corpus is
  ordinary. Anything proposed here that improves steady state at the cost of
  the transient peak has the trade backwards.

## What "done" means

mailrs can load their 95 MB seed without the import process threatening the
machine. A number rather than a direction: **peak RSS under 1 GB for the full
schema**, against 2,614 MB today — and stated as peak, because the resident
cost afterwards is already 256 MB and was never what they hit.

If B1 and B2 get there, 7.37.18 is those two, the measurements above, and an
honest note that the posting-list encoder this document was named for was not
needed. If they do not, the remaining ~260 MB has to be named first.
