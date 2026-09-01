# Embedded vs Server — the hard boundary

Two hosts, one engine, one disk format. `spg-embedded` links the
engine into your process; `spg-server` wraps the same engine in a
network daemon. The SQL surface (parser, executor, triggers,
aggregates, types) is the SAME crate (`spg-engine`) — a dialect fix
lands on both sides by construction. What differs is everything
AROUND the engine: processes, sessions, transactions-on-the-wire,
and operational features. This document is the authoritative list.

Updated 2026-08-07. The four items that carried an **[AUDIT]** marker —
open questions about edges nobody had checked — were settled in rounds
802 and 804 and now say what was measured instead of what was suspected.
Two were stale worries (session state does not leak between connections;
wire COPY does honour column lists), one was a real divergence and is
fixed (multi-statement messages are one implicit transaction, round 803),
and one is a real limit that fails safely (embedded's `0x12` records stop
the server's replay with an error rather than being skipped).

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
| Implicit transactions | `execute_script` wraps multi-statement scripts (PG simple-query semantics); `spg import` is atomic. | Audited round 802, and it diverges. PG wraps the whole message in one implicit transaction: `INSERT INTO t VALUES (1); INSERT INTO t VALUES (1);` as a single Q message against a PK leaves **zero** rows — the first insert rolls back with the second. SPG dispatches per statement and leaves **one**. A migration script that fails halfway is therefore half-applied here and not applied at all on PG. Fixing it has to account for the round-794 rule that VACUUM / CREATE DATABASE / ALTER SYSTEM cannot run inside a transaction block — PG allows them in a simple query only when they are the sole statement in the message. |
| Session state | One `Database` = one session. The string-dialect switch (`SET sql_mode` / `standard_conforming_strings`, v7.22) flips the engine's lexer mode and clears the plan cache. | Measured round 802 and it does not leak: with connection B setting `standard_conforming_strings = off` and then `sql_mode = 'NO_BACKSLASH_ESCAPES'` — two different branches of the dialect switch — connection A kept `on` throughout and `E'a\\tb'` kept lexing the same both times. The flag is still a field on the shared `Engine`, so the isolation comes from the per-connection settings the server applies around each statement rather than from the engine owning one per session; a future session-scoped GUC has to go through that same path to inherit it. |
| COPY | `COPY … FROM stdin` text blocks via `spg import` / `execute_script` (pg_dump default format). | Wire COPY protocol with SKIP / ON_ERROR / JSON options; column lists are honoured — checked round 804 against PG 18.4 with the same script on both, and they agree row for row: `COPY t (b, a)` assigns by the list rather than by table order, `COPY t (c, a)` leaves the omitted column NULL, and a column carrying a DEFAULT gets the supplied value when the list names it. (The older note here said the wire head ignored them; it no longer does.) |
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
- embedded WAL → server open: the two hosts do not share a WAL layout at
  all, so this never reaches the question of record types. Measured round
  804: embedded leaves `<db>.wal/` — a directory of segment files such as
  `000658683f4445c0_0000000000000001.wal` — while the server is given a
  single WAL file on its command line and exits at startup with
  `fatal: Is a directory` when pointed at embedded's. The failure is
  loud, which is the safe kind, but it arrives before any `0x12`
  whole-transaction record is ever parsed.

  (An earlier revision of this line claimed the server refuses these
  records via its unknown-type arm. That arm belongs to the v3 envelope
  and embedded writes v4, so the reasoning did not apply — the layout
  mismatch above is what actually happens.)

  The supported handoff is therefore unchanged and it is not a
  formality: **checkpoint first** (graceful close folds the WAL into the
  snapshot), then open under the other host. `spgctl wal-lint` verifies
  a WAL before handoff.

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
image = the server path). A capability proven on one host only is
treated as unshipped.

## Open audit items (the not-yet-sharp edges)

1. Wire multi-statement implicit transaction (backlog P0-2).
2. `SET sql_mode` / session-GUC scope on a shared multi-connection
   engine (new, from this document's review).
3. Embedded `0x12` WAL records under the server's replay path
   (cross-host open test).
4. Wire COPY column lists (backlog P1-9).
5. Error-message/SQLSTATE parity between hosts (rounds 12-20 N2).
