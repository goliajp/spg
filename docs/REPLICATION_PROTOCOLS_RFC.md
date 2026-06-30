# MAGIC_SUB + MAGIC_REPL integration RFC (v7.37.25.9)

> **Status: RFC.** Captures the integration design without changing
> wire protocols. Implementation lands when a customer dogfood gate
> actually exercises both shapes in one cluster.

## Today's surface (4 magics)

| Magic               | Bytes                  | Shipped | Purpose |
|---------------------|------------------------|---------|---------|
| `MAGIC_V1` (REPL)   | `b"SPGREPL\x01"`      | v4.24   | Raw-WAL streaming, no framing, no lag |
| `MAGIC_V2` (REPL)   | `b"SPGREPL\x02"`      | v4.36   | Framed WAL + `0x01` status frame for lag metrics |
| `MAGIC_SUB`         | `b"SPGSUB\x01\x00"`   | v6.1.4  | PG-style logical-replication subscriber — skip snapshot, tail-from-current, filter by publication |
| (HTTP admin)        | n/a                    | various | `/metrics`, `/readyz`, etc. — out of scope here |

## What "integration" means

Three open questions today:

1. **Can one TCP listener serve both REPL and SUB?** Today's
   `start_replication_server` reads the first 8 bytes, dispatches on
   magic. A single listener already serves all three; no listener-side
   work is needed.

2. **Can a follower simultaneously be a subscriber?** Mixed-shape
   clusters (some links are physical mirrors, others are filtered
   logical subscriptions) are the real driver. Today this works at
   the cluster level (server A follows server B via REPL; server A
   also subscribes to server C via SUB) but the two states are
   tracked independently in `state.lag_state` and
   `state.subscriber_state`. No single dashboard sees both.

3. **Can MAGIC_SUB carry status frames?** Today only REPL `0x01`
   status frames feed `spg_replication_lag_*` metrics. MAGIC_SUB
   could honour the same frame type for free since the post-handshake
   frame format is shared.

## RFC proposal — three discrete moves

### Move 1: shared status-frame semantics

MAGIC_SUB streams currently never emit `0x01` status frames. Wire
them up so subscribers report lag the same way followers do. The
follower / subscriber surfaces converge under one set of
`spg_replication_lag_bytes{shape="repl"|"sub"}` labels.

Cost: ~30 LOC on the publisher side (already emit `0x01` in the REPL
path; copy the emit into the SUB path). Zero wire-format change.

### Move 2: unified `pg_stat_replication` row shape

Today `pg_stat_replication` (v7.37.21.13-d) reports REPL followers.
Move 2 widens the query to UNION SUB subscribers, distinguished by
`application_name` (REPL: `walreceiver`-like; SUB: the publication
name). The `state` column reflects `streaming` for both.

Cost: ~50 LOC in `synth_pg_stat_replication`; surface-level only.

### Move 3: ALTER SUBSCRIPTION reload

Today `CREATE SUBSCRIPTION` builds an immutable subscription record.
`ALTER SUBSCRIPTION s SET (...)` (publication list change, connection
string change) is a parse-and-no-op. Move 3 honours the publication
list change by reopening the subscriber TCP stream with the new
filter on the next reconnect window.

Cost: ~150 LOC including reconnect plumbing. Wire format change:
SUB handshake reply needs a `publication_list_version` field so the
subscriber can detect the publisher's view drifted. Bumps SUB magic
to `b"SPGSUB\x01\x01"`.

## Why this is RFC not implementation

- Moves 1 + 2 are clean and cheap, but neither solves a customer
  problem today (mailrs / sentori don't ship multi-shape clusters).
- Move 3 is real wire format work and gates on the operational shape
  of "I want subscriber-side ALTER SUBSCRIPTION to reflect publisher
  changes without a follower restart" — a reasonable ask, not yet a
  customer ask.

When a customer demand materializes (operationally meaning a single
SPG cluster runs both a physical mirror leg and a publication leg
and the operator wants one dashboard view), revisit and pull the
relevant move(s) into the next release. The RFC is the queue-card.

## Open questions

- Does MAGIC_SUB need a separate snapshot-skip negotiation when the
  publication includes a row-filter (`publication_row_filter`, 21.2)?
  The filter could mean "stream tail records subject to filter
  predicate"; today the filter is pre-publisher-side and the
  subscriber sees only filtered records, so no extra negotiation.
- Does Move 3's wire format bump conflict with v7.37.21.15 Hot
  Standby physical replication (which depends on 15.x snapshot)?
  Hot Standby would use MAGIC_REPL (physical), not MAGIC_SUB
  (logical), so no clash. Document the partition explicitly when
  21.15 lands.

## See also

- `crates/spg-server/src/replication.rs` — `MAGIC_V1`, `MAGIC_V2`,
  `MAGIC_SUB` constants + dispatcher
- `docs/WIRE_FORMAT_PROMISE.md` — tier-1 commitment for pgwire,
  not replication wire; replication wire is dev-tier (no
  cross-version compat commitment yet — 21.1)
- v7.37.21.13 / 21.13-b / 21.13-c — pg_replication_slots,
  pg_publication, pg_subscription views (today's monitoring surface)
