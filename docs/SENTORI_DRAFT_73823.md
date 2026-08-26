# spg → sentori — 7.38.23, and the bill 7.38.22 sent you

**Image:** `goliakk/spg:7.38.23` (draft — not built yet)
**Manifest digest:** (filled in by the release train)
**Battery:** nine-step prerelease green on the exclusive testbed; drop-in
acceptance 71/71 against the built image.

Nothing you reported is involved and there is nothing for you to change.
This note exists because the last one announced a fix and did not
mention what the fix cost.

## What we told you, and what we left out

7.38.22 made a declared collation reach the sort that spills, which it
never had. That note gave the cost as 1.41x — 164.8 ms against 232.5 on
400,000 rows.

**That number was for the wrong shape.** It came from a cell that sorts
entirely in memory. A sort that RETURNS its rows spills, takes the other
implementation inside us, and cost more. We had no cell for it, which is
exactly why the number we had was the one we gave you.

Measured since, inside the published 7.38.22 image, with an explicit
`COLLATE "C"` as the control so both legs run in one window:

    SELECT s FROM t ORDER BY s        400,000 rows of 192-character text
    under COLLATE "C"                 274 ms   (256-315 over six pairs)
    under the image's own default     639 ms   (623-654)
                                               2.36x  (2.02-2.44)

The image's default is `en_US.utf8` as of 7.38.22, so that is what a
plain `ORDER BY` on a text column has been costing you since you took
that image, **with nothing declared on your side**. On 7.38.21 the same
query was free, because it was ignoring the collation — which is the
defect 7.38.22 fixed.

## What it was

Our spilling sorter does not write its sort keys to disk. It re-derives
them from the row as it merges. 7.38.22 gave the sorter its collations
and did not give them to the half that re-derives — so one sort built
its keys two ways. The pusher marked a value drawn from `[0-9a-z]` as
already in byte order; the merge, handed nothing, did not, and every
comparison in the merge re-derived what the other half had established.

It was never a wrong answer. Both halves agree with PostgreSQL 18.4 row
for row, before and after. Only the clock could see it.

## Where it stands now

Same query, same fixture, on the binary this release ships:

    declaring a collation that orders this data exactly as bytes do
      7.38.22          2.91x
      7.38.23          0.99x   (spread 0.96-1.08)

**Free.** And text that genuinely needs a collator — anything with a
capital, an accent or a space — pays what it paid before: 9.22x against
no collation at all (spread 7.52-9.58), against 8.52x on 7.38.22
(7.75-9.15). The spreads overlap, which is the whole claim: nothing was
moved onto the values that need a collator.

Our release panel now holds the question as a gate: the same binary
under `en_US.utf8` against itself under `C`, with a 2.0x ceiling on the
sort cells. 7.38.22 measures 3.09x on it and fails; this release
measures 1.08x and 1.32x across two runs whose own control did not
fire.

## What we are not claiming

The timings are ours, on our testbed, 400,000 rows, `work_mem = 4MB`.
They are the shapes we test, not yours. If a sort of yours is wide or
long enough to spill and you have a text key, this release is the one
that reaches it — and if you can tell us the shape and the row count, it
goes in the panel.
