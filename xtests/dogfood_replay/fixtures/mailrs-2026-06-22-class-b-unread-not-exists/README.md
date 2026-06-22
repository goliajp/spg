# mailrs 2026-06-22 — Class B unread tally (NOT EXISTS anti-join)

## Prod context

Filed by mailrs: `stables/mailrs/.claude/notes/spg-7.37.6-prod-pool-cascade-4th-recurrence-2026-06-22.md`
**4th recurrence of the same cascade**. Class B contributed 8 of the 25 budget
breaches in the 2h pre-restart window. It is the simplest of the three classes;
the report explicitly flags this fixture as the highest-leverage starting point:

> 1. Class B EXPLAIN ANALYZE on prod-shape catalog.
> The simplest of the three; if spg's anti-join optimization doesn't fire here,
> no amount of fix on Class A/C will matter.

## Shape

Single-table COUNT(*) on `messages` with a bitwise flag predicate (`flags & 1 = 0`,
i.e. unread) and a correlated `NOT EXISTS` subquery against `email_analysis`. The
inner predicate is `category IN ('spam', 'scam')`. PG18 lowers this to a hash-anti
or merge-anti join; SPG's current planner reportedly drives the subquery per row
on prod catalog shape, hence the 3.6s avg.

The fixture binds `mailbox_id` via an inline scalar subquery so it is
self-contained without external parameter substitution. Both PG18 and SPG should
fold the scalar subquery to a constant before executing the outer COUNT.

## Snapshot

Shared with Track A — same prod catalog (06-20, 235 MB tarball, 825 MB extracted).
SHA-256: `f0ad88ba16b11ec2f7b53a0e50da59e20099d0ed472d061edb6bc80c195a609e`.

The `snapshot.tar.gz` in this directory is a symlink to
`../mailrs-2026-06-22-track-a/snapshot.tar.gz`; the runner's SHA-256 verification
sees through the symlink and validates the same payload.

## Budget posture

Initial budgets are **loose** (cold ≤ 200 ms, warm ≤ 100 ms, p95 ≤ 150 ms)
pending real measurement. After Phase A decomposition + Phase B attack lands,
tighten to PG18 parity (cold ≤ 100 ms per the mailrs report's PG baseline,
warm and p95 commensurately).

## Workflow gates

Per `docs/PERF_METHODOLOGY_VS_FOSS.md`:

1. **Phase A — decomposition (read-only)**. Read PG18 `nodeAgg.c` /
   `nodeHashJoin.c` / `nodeNestloop.c` equivalents for the
   COUNT(*) + anti-join shape. Write 18+ stage decomposition (PG side × SPG
   side, atomic op counts, file:line, ±20% reconciled against measured wire
   time). Produce Top-N actionable attacks.
2. **Phase B — attack (worktree-isolated)**. Apply attacks atomically; bench
   validate cumulative.
3. **Phase C — re-bench 13-shape sweep**. Ensure no regression on other shapes.

Decomp doc: `.claude/notes/v7.37.7-mailrs-cascade-class-b-decomp.md` (to be
authored as Phase A's first step).

## DO NOT

- DO NOT polish before decomposition. Per [[feedback-perf-hard-means-do-micro-decompose]]:
  2 rounds of unmoved-needle polish = automatic STOP, switch to decomposition.
- DO NOT claim "structural ceiling" / "noise band" / "workload not amenable"
  without a completed decomposition (the v7.37.5 ack already triggered this
  self-deception in writing).
- DO NOT use synthetic seed data as a substitute for the prod snapshot.
