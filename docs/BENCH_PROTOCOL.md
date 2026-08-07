# Bench protocol — how a "SPG vs PG" number is allowed to be produced

Every rule here was written the day a measurement lied and a decision
was made on it. None is precautionary.

## The rule

**A cross-implementation timing is invalid unless the client, the
workload and the run count are identical on both sides, and the setup
was verified before the timing was read.**

## The checks, and what each one cost

### 1. Same client on both sides

Round 841 measured SPG at 0.83 s and PG at 0.40 s for
`SELECT pad FROM big ORDER BY id` over 400k rows, and reported a 2.1x
loss. SPG had been driven by a Python probe that parses 400k DataRow
messages in Python; PG by `psql`, which is C. Re-run through `psql` on
both sides, SPG is 0.46–0.54 s — the gap is 1.15–1.35x and roughly
0.35 s of the original number was the probe.

The same contamination had already been passed upward: round 837
reported the spilling sort as **5.4x slower than PG** and that number
was used to hold the wiring back. Its real figure is 1.13–1.4x.

- Drive both sides with `psql` for wall-clock comparisons.
- A raw-protocol probe is for **correctness and shape**, never for
  cross-implementation timing.
- If a probe must be timed, time the SAME probe against both.

### 2. Verify the setup produced data before reading a timing

Round 841 read `Execution Time: 0.023 ms` from PG for a 400k-row sort
and nearly concluded PG was 50x faster. The heredoc quoting had failed
and the table was empty.

- Print `count(*)` (or the row count the run returns) BEFORE the timing.
- A number that is impossibly good is a broken probe until proven
  otherwise.

### 3. Check the plan, not just the SQL

Round 841 compared `ORDER BY id` where `id` is a PRIMARY KEY. PG never
sorted it — it walked the index, and `Sort Method` never appeared in
`EXPLAIN ANALYZE`. Sort-vs-index is not a sort comparison.

- For a sort comparison, order by a column with **no index**.
- Read `EXPLAIN (ANALYZE)` and confirm the operator you meant to measure
  is in the plan (`Sort Method: external merge  Disk: …` for a spill).

### 4. Interleave, repeat, and report the spread

The testbed's load drifts 5–10% on its own, so "run all of A then all of
B" attributes drift to whichever ran second.

- n ≥ 3 per side, alternating A/B/A/B, and flip the starting side.
- Report the spread, not one number. **Overlapping ranges mean no
  measured difference** — round 841's spilled (0.45–0.56 s) and
  unspilled (0.46–0.54 s) overlap, so the spill's cost is not
  measurable at this size, whatever the midpoints suggest.

### 5. Separate the server's time from the wire's

`SELECT id` (10 MB on the wire) and `SELECT pad` (80 MB) cost the same
0.96 s vs 1.01 s through the Python probe: the cost was per ROW, not per
byte, and most of it was the client.

- Use `EXPLAIN (ANALYZE)`'s `Execution Time` for server-side work.
- Use wall clock for what a client actually experiences.
- Say which one a number is. They answer different questions.

### 6. Read the machine's load before, and again after

The testbed is shared. Another project's test binary took 1193% CPU for
six minutes and pushed the load average to 33, and every benchmark taken
in that window slowed down together — including two that the change under
test could not reach: an unchanged probe went from 15.1 ms to 38.6 ms,
2.6x, measuring nothing but the neighbour.

That is the tell, and it is worth more than the load number itself:

- Keep an unchanged benchmark in the same run as a **control**. If the
  control moves, the run is contaminated and none of it counts, however
  plausible the changed number looks.
- `uptime` before and after. Load above roughly the core count means
  re-measure later rather than reason about the reading.
- Sweep your own leftovers first — a probe's server that outlived its
  `kill` was still resident an hour later.

Rule 4 does not cover this. Interleaving handles drift *between* two
sides measured in one window; it cannot rescue a window where everything
moved at once.

## Before quoting a number in a decision

- [ ] Same client both sides?
- [ ] Row count verified before timing?
- [ ] Plan contains the operator being measured?
- [ ] n ≥ 3, interleaved, spread reported?
- [ ] Server-side vs wall clock stated?
- [ ] Machine quiet, and an unchanged control held still?

A number that fails any of these is not evidence. Re-measure or say it
is unknown — an honest "not measured" costs one round; a decision made
on a contaminated number costs the rounds spent acting on it, and this
session spent two.
