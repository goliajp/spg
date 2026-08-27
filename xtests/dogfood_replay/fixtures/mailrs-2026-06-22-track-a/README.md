# mailrs 2026-06-22 — Track A `/api/conversations`

## Prod context

Filed by: `stables/mailrs/.claude/notes/spg-7.37.3-prod-conversations-real-sql-2026-06-18.md`

Track A is the `/api/conversations` listing query — the slowest
single statement in the mailrs hot path. The query stitches
`conversations × messages × labels` with a windowed ranking and a
`NOT EXISTS` filter against the spam label.

## Budgets

`13135db9` replaced this fixture's query. The original was a hand-written
CTE against tables the prod snapshot does not have (`conversations`,
`c.user_id`, `c.deleted_at`), so it had never run; the real
`/api/conversations` SQL took its place and the budgets were re-locked
around it. This table is generated from `fixture.json` and kept honest by
`every_query_fixture_readme_carries_the_budget_the_gate_reads`.

<!-- BUDGETS: generated from fixture.json — the gate reads the JSON, not this table -->
| Window | Budget |
| --- | --- |
| Cold (first iter) | ≤ 100 ms |
| Warm median (p50) | ≤ 85 ms |
| p95 | ≤ 90 ms |
<!-- /BUDGETS -->

## Snapshot

The Track A snapshot is the same prod catalog as Track B/D — replace
`sha256` in `fixture.json` with the real digest once the tarball is
published. The current zeros are a stub so `verify` reports
`MISSING`, not `CORRUPT`, against an unpublished tarball.
