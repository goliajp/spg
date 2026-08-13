# v7.37.18 — the memory half of mailrs's report

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

## Phase B — the change, ordered so the risky step is small

Only if A says posting lists are a double-digit share of steady-state RSS.

### B0 — decide the representation against the read path

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

### B1 — change the API shape first, keeping the representation

`Index::gin_lookup_word`, `gin_trgm_lookup`, `gin_jsonb_lookup` all return
`&[RowLocator]`, which a compressed list cannot produce. Turn them into
iterators **while still backed by `Vec<RowLocator>`**.

Zero behaviour change, so the full gate proves the refactor on its own. Every
caller is then already shaped for a compressed backing, and the step that
actually changes representation touches storage internals only.

### B2 — narrow the element

Hot locators are row indices. `Vec<u32>` for the hot postings plus a rare
side list for cold ones is 16 → 4 bytes with no encoder at all. Land and
measure this before B3: if it takes 2.87 GB to something acceptable, B3 may
not be needed, and 4× for a mechanical change is a good trade against the
complexity of an encoder.

### B3 — encode

Delta + varint + skip index, behind the B1 iterator. Append becomes
"append to the tail block"; the structure stays append-friendly because
posting lists are built in ascending row order — which is exactly the
property r1018's in-place append relies on, so it is already true.

Deletes and vacuum rewrite blocks rather than removing an element in place.
Cost is bounded by block size; measure it against the autovacuum gate.

### B4 — the on-disk form

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

- **A does not reconcile to ±20 %.** Do not start B. Find the missing term.
- **A says rows, not postings, dominate.** This document is the wrong plan;
  write the row-storage one.
- **A says the 2× is `Vec` capacity slack.** Ship `shrink_to_fit` at quiesce
  and re-measure before committing to an encoder.
- **B2 alone reaches an acceptable number.** Stop there and say so; an
  encoder that buys nothing is complexity with a story attached.
- **B0's bench says intersection regresses materially** and a skip index does
  not recover it. Then the representation is wrong for this workload and
  roaring gets its turn.

---

## What "done" means

mailrs loads their 95 MB file and the resident cost is proportional to what a
database would hold, not 30× the input. A number to beat rather than a
direction: **under 1 GB for the full seed**, with the read-side gate green.
If Phase A shows that number is not reachable without also changing row
storage, then it says so, and 7.37.18 carries the part that is reachable
along with the measured reason for the rest.
