# Embedded vs Server — the hard boundary

Two hosts, one engine, one disk format. `spg-embedded` links the
engine into your process; `spg-server` wraps the same engine in a
network daemon. The SQL surface (parser, executor, triggers,
aggregates, types) is the SAME crate (`spg-engine`) — a dialect fix
lands on both sides by construction. What differs is everything
AROUND the engine: processes, sessions, transactions-on-the-wire,
and operational features. This document is the authoritative list.

Updated 2026-06-11 (post rounds 12-20). Items marked **[AUDIT]**
are known-open questions, tracked in the backlog — they are the
edges where the boundary is not yet proven sharp.

## What is shared (and therefore identical)

| Layer | Crate | Notes |
|---|---|---|
| SQL parse/execute | `spg-engine` + `spg-sql` | Every shape in the rounds 12-20 panels runs on both hosts; the dropin panel pins the server path, the e2e suites pin the embedded paths. |
| Snapshot format | `spg-storage` (FILE_VERSION 46) | Byte-identical. |
| Cold segments | `spg-storage` (inner magic V3) | Byte-identical. |
| Manifest | `spg-manifest` | Extracted into its own crate (v7.1) precisely so both hosts read/write the same bytes. |
| Release gates | all four | sqllogictest + dump-compat (wire AND import pass) + data-compat + zero-change run against both hosts every release. |

**Disk compatibility is a design contract**: a data directory
written by one host opens under the other, with one transaction-log
caveat below.

## The hard differences

| Axis | Embedded | Server |
|---|---|---|
| Process model | In your process; one OS process owns the files (pid lock; `force_unlock` for crash recovery). | Own daemon; clients over TCP. |
| Protocols | Rust API (+ sqlx 0.8 driver). | spgwire (native), pgwire (any PG client), mysqlwire (opt-in). |
| Concurrency | Single writer (`&mut Database`); lock-free readers via catalog snapshots (`prepare/execute_prepared_on_snapshot` — the sqlx readonly-inline path). | Many connections; writer lock inside the daemon; group commit; parallel prepare; background freezer + boot prefetch pool. |
| Transactions on disk | Atomic commit record: the whole transaction's bind-final SQL is buffered and written as ONE WAL record (`0x12`) at COMMIT; savepoints truncate the buffer. | Per-statement WAL: the BEGIN/…/COMMIT verb stream is logged statement-by-statement with `sync_data`. |
| Implicit transactions | `execute_script` wraps multi-statement scripts (PG simple-query semantics); `spg import` is atomic. | **[AUDIT]** multi-statement simple-query messages: PG wraps the whole message in one implicit transaction; the server's per-statement dispatch has not been audited against this (backlog P0-2). |
| Session state | One `Database` = one session. The string-dialect switch (`SET sql_mode` / `standard_conforming_strings`, v7.22) flips the engine's lexer mode and clears the plan cache. | **[AUDIT]** the dialect flag lives on the shared `Engine`; with many connections a `SET sql_mode` from one connection may leak to all sessions. Same question for any future session-scoped GUC. (The pgwire layer currently reports `standard_conforming_strings = on` as a fixed parameter status.) |
| COPY | `COPY … FROM stdin` text blocks via `spg import` / `execute_script` (pg_dump default format). | Wire COPY protocol with SKIP / ON_ERROR / JSON options; column lists **[AUDIT]** ignored on the wire head (engine helper supports them; backlog P1-9). |
| Replication / HA | None (single process by definition). | Segment forwarding + follower bootstrap/resume, `WAIT FOR WAL POSITION`. |
| Observability | Rust API; `pg_trigger`/`pg_catalog` synth tables (shared). | Same synth tables + HTTP `/metrics` (Prometheus), `spg_stat_segment`, SLO smoke gate. |
| Ops surface | `spgctl` against the file (`import`, `backup`, `restore`, `wal-lint`, `revert`). | Same `spgctl` over the socket (`ping`, `query`, `stats`) + graceful SIGTERM, disk water-marks. |
| Latency | No network, no serialisation: µs-scale point reads (the headline). | Wire round-trip + encode/decode on every call. |

## Transaction-log interchange — the one disk caveat

Snapshots, segments and the manifest interchange freely. The WAL
both hosts write is the same v4 record format (SQL text records),
**but the transaction representation differs**: embedded writes the
atomic `0x12` commit record; the server logs the verb stream.

- server WAL → embedded open: replays (BEGIN/…/COMMIT statements
  re-execute through the engine's tx machinery).
- embedded WAL → server open: **[AUDIT]** requires the server's
  replay path to accept `0x12` records. Until that is pinned by a
  cross-host test, the supported handoff is: **checkpoint first**
  (graceful close folds the WAL into the snapshot), then open under
  the other host. `spgctl wal-lint` verifies a WAL before handoff.

This is the recommended migration recipe in both directions:
close cleanly (or `spgctl backup`), move the directory, open.

## Choosing (customer guidance)

- **Embedded** when the database belongs to one service instance
  and latency is the point: in-process reads skip the network and
  the codec entirely; snapshot reads don't even take the writer
  lock. This is SPG's headline deployment (mailrs runs prod this
  way).
- **Server** when more than one process/language needs the data,
  when you want drop-in PG/MySQL wire compatibility for existing
  clients, or when you need replication and Prometheus metrics.
- Mixed: develop embedded, expose read-only tooling via a server
  pointed at a backup/restored copy — not at the SAME live
  directory (one-writer rule is per-directory, enforced by the pid
  lock).

## Parity policy (what "same capability" means)

Every dialect/engine capability ships on both hosts by construction
(shared engine). Host-level features are allowed to differ (table
above), but **acceptance shapes must be pinned on both**: the
embedded e2e suites AND the dropin panel (psql against the docker
image = the server path) — see `docs/TESTING.md` § acceptance-shape
conventions. A capability proven on one host only is treated as
unshipped.

## Open audit items (the not-yet-sharp edges)

1. Wire multi-statement implicit transaction (backlog P0-2).
2. `SET sql_mode` / session-GUC scope on a shared multi-connection
   engine (new, from this document's review).
3. Embedded `0x12` WAL records under the server's replay path
   (cross-host open test).
4. Wire COPY column lists (backlog P1-9).
5. Error-message/SQLSTATE parity between hosts (rounds 12-20 N2).
