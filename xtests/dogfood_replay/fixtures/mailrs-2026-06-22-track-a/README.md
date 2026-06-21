# mailrs 2026-06-22 — Track A `/api/conversations`

## Prod context

Filed by: `stables/mailrs/.claude/notes/spg-7.37.3-prod-conversations-real-sql-2026-06-18.md`

Track A is the `/api/conversations` listing query — the slowest
single statement in the mailrs hot path. The query stitches
`conversations × messages × labels` with a windowed ranking and a
`NOT EXISTS` filter against the spam label.

## Expected post-fix

| Window   | Warm median | p95     | Cold first iter |
| -------- | ----------- | ------- | --------------- |
| Post-fix | ≤ 5 ms      | ≤ 10 ms | ≤ 100 ms        |

## Snapshot

The Track A snapshot is the same prod catalog as Track B/D — replace
`sha256` in `fixture.json` with the real digest once the tarball is
published. The current zeros are a stub so `verify` reports
`MISSING`, not `CORRUPT`, against an unpublished tarball.
