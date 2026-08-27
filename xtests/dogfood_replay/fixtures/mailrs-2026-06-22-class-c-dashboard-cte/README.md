# mailrs 2026-06-22 — Class C unread-thread dashboard CTE

## Prod context

Filed by mailrs: `stables/mailrs/.claude/notes/spg-7.37.6-prod-pool-cascade-4th-recurrence-2026-06-22.md`
**4th recurrence**. Class C is the dashboard counter (homepage); 3 hits in the
2h pre-restart window. Each hit averaged 3.6s — every dashboard refresh adds
one to the cascade.

## Shape (mailrs comment, verbatim)

> A note from the source comment (`read.rs:204-210`): this is **already a
> rewrite** of the cleaner standard-SQL form (FROM-clause derived table +
> FILTER aggregate) that spg 7.30.3 couldn't parse. mailrs is paying a
> readability tax to stay compatible. The rewrite parses but performs
> poorly on prod catalog shape.

Composite shape: CTE wraps `messages × mailboxes` JOIN with 2 NOT EXISTS
subqueries (`snoozed_conversations` time-bounded + `email_analysis` IN-list
spam/scam), then HAVING with `BOOL_OR(archived) = false` AND
`COUNT(CASE WHEN flags & 1 = 0 THEN 1) > 0` AND
`LOWER(COALESCE((SELECT … FROM messages ORDER BY internal_date DESC LIMIT 1), '')) NOT LIKE '%' || LOWER($1) || '%'`,
then outer COUNT(*) over the CTE.

## Snapshot

Shared with Track A — symlinked. SHA-256
`f0ad88ba16b11ec2f7b53a0e50da59e20099d0ed472d061edb6bc80c195a609e`.

## Budget posture

Still the **loose** opening budgets, pending measurement. After decomp +
attack, tighten to PG18 parity.

<!-- BUDGETS: generated from fixture.json — the gate reads the JSON, not this table -->
| Window | Budget |
| --- | --- |
| Cold (first iter) | ≤ 300 ms |
| Warm median (p50) | ≤ 200 ms |
| p95 | ≤ 250 ms |
<!-- /BUDGETS -->

## Workflow gates

Per `docs/PERF_METHODOLOGY_VS_FOSS.md` Phase A → Phase B. Decomp doc to
follow after Class B baseline is recorded.

## Discipline reminders

- No "structural ceiling" framing without completed decomposition.
- No claiming "the rewrite is the limit" without measuring the rewrite at
  PG18 on the same data first.
- The mailrs report names this query specifically because the rewrite cost
  was paid for spg-side parser gaps — owning the perf gap on the rewritten
  shape is non-negotiable per the vision rule (any angle ≥ PG).
