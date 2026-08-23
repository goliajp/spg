# Recorded deltas — where SPG knowingly answers differently from PostgreSQL 18

A "recorded delta" is a comment in the source saying *we know this
differs and here is why*. There were seventeen such comments and no list,
so nobody could read them all, and none of them had been re-measured
since the day it was written.

**That is the same defect as a compatibility matrix nobody re-runs**, and
it cost the same way. Re-measuring the nine markers in `crates/*/src`
against a live PostgreSQL 18.4 on 2026-08-23:

- one **does not reproduce** — the behaviour it described is gone
- one is **stated backwards** — the divergence is real and at the other end
- one is **closed** — both engines now answer identically
- two are **open** as written
- and the exercise turned up **one the comments never mentioned**, in the
  direction that matters most: SPG accepting what PostgreSQL rejects

The register is checked by the gate: a marker in the source with no row
here is red, and a row here whose marker has gone is red. A delta that
cannot be forgotten is a delta someone will eventually close.

## Open

| id | where | PostgreSQL 18.4 | SPG | measured |
|---|---|---|---|---|
| RD-1 | `eval/cast.rs` — `pg_typeof(x::cstring)` | `cstring` | `text` | 2026-08-23 |
| RD-2 | `eval/binop.rs` — `'infinity'::timestamp - '-infinity'::timestamp` | `infinity` | `ERROR: interval out of range` | 2026-08-23 |
| RD-3 | `eval/binop.rs` — `'294276-12-31 23:59:59'::timestamp` | accepted | `ERROR: date/time field value out of range` | 2026-08-23 |
| RD-4 | `transaction.rs` — a concurrent UPDATE's re-check | re-applies to the winner's new version (EvalPlanQual) | the UPDATE matches zero rows | not re-measured; needs two live sessions |
| RD-5 | `explain.rs` — `EXPLAIN (ANALYZE, BUFFERS)` sort line | reports a peak | SPG does not meter one | not re-measured; a number that was not measured is worse than none |
| RD-6 | `parser.rs` — `json_populate_record` with a non-NULL record base | takes the base's field values as defaults | the base carries only the type | not re-measured; needs a composite fixture |

## Corrected by measurement

| id | what the comment said | what is true | measured |
|---|---|---|---|
| RD-7 | `parser.rs` — a numeric literal deeper than SPG's `u8` scale falls back to double precision | does not reproduce: 300 fractional digits round-trip identically on both | 2026-08-23 |
| RD-8 | `eval/binop.rs` — "SPG's 1970-based clock ends ~30 years before PG's 2000-based ceiling" | the *lower* bound agrees (`4714-11-24 BC` on both); it is the **upper** bound that differs, and by far more than thirty years — this is RD-3 | 2026-08-23 |
| RD-9 | `eval/cast.rs` — `::xid` carried as BigInt | closed: `1::xid` errors identically on both (`cannot cast type integer to xid`) | 2026-08-23 |
| RD-10 | `parser.rs` — `SET SESSION AUTHORIZATION` moves the effective role where PG moves `session_user` | both engines leave `current_user` and `session_user` unchanged for `DEFAULT`; the claim needs a non-default role to test and is unproven as written | 2026-08-23 |

## Found while re-measuring, and CLOSED in the same version

| id | what it was | now |
|---|---|---|
| RD-11 | `SELECT 1::xid8` returned `1` where PG answers `ERROR: cannot cast type integer to xid8` — **SPG accepting what PG rejects**, the worse direction, because code PG would have stopped runs here and the difference surfaces somewhere else, later | every integer type is refused for both `xid` and `xid8`, and the text form still works. Six forms measured against PG 18.4 and identical: `1::xid8`, `1::bigint::xid8`, `1::xid`, `'1'::text::xid8`, `'42'::xid8`, `'7'::xid` |

Nobody had recorded RD-11. It surfaced only because the comment two
lines above it was being re-measured — which is the argument for this
file existing.

## The rule

A comment that records a divergence must carry an `RD-n` id from this
table. When you close one, delete its row and its marker in the same
commit. When you add one, measure both engines first and paste what they
answered — not what you expect them to answer.
