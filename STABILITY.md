# SPG stability contract

What's frozen, what's not, how to read a version bump.

---

## SemVer for SPG

- **MAJOR** bump → breaking change. Old clients may not connect;
  old snapshot/WAL files may need migration. None planned in
  v4.x.
- **MINOR** bump → backwards-compatible additions. New SQL
  features, new wire opcodes, new env vars. Old behaviors
  unchanged.
- **PATCH** bump → bug fix or doc-only. Wire / SQL / file formats
  unchanged.

Pre-v1.0 caveat: SPG itself is at v4.x, but treats this as its
"v1.0 contract" for the surfaces below. A v5.0 release would be
the first formal breaking-change window.

---

## Frozen surfaces

These are stable: client code, snapshot files, and operational
config written against the contract continue to work against any
future v4.x release. v4.31's CI gates this for the snapshot
format; the others are gated by per-row `prod_ready` tests.

### Native wire protocol (port `SPG_ADDR`)

The 13 opcodes are fixed. Adding new opcodes is a MINOR bump;
changing semantics or layout of an existing opcode is MAJOR.

| op   | name             | direction | added  |
|-----:|------------------|-----------|--------|
| 0x00 | Ping             | client→   | v0.1   |
| 0x01 | Pong             | server→   | v0.1   |
| 0x02 | Auth             | client→   | v1.14  |
| 0x03 | AuthUser         | client→   | v4.1   |
| 0x10 | Query            | client→   | v0.5   |
| 0x11 | RowDescription   | server→   | v0.5   |
| 0x12 | DataRow          | server→   | v0.5   |
| 0x13 | CommandComplete  | server→   | v0.5   |
| 0x14 | ErrorResponse    | server→   | v0.5   |
| 0x15 | Stats            | client→   | v1.0   |
| 0x16 | StatsResponse    | server→   | v1.0   |
| 0x17 | DataRowBatch     | server→   | v3.3.0 |
| 0xFF | Error            | server→   | v0.1   |

Frame layout (every opcode): `[u32 LE payload_len][u8 op][payload bytes]`.

The `prod_ready` row 8.2 test reads back this table and fails
the build if any opcode byte or name changes.

### PostgreSQL wire protocol (port `SPG_PG_ADDR`)

SPG implements the documented subset of PostgreSQL v3 protocol.
The protocol itself is defined by upstream PostgreSQL; SPG never
extends it with custom messages. Stability of this surface is
the stability of PG v3 — see
<https://www.postgresql.org/docs/current/protocol.html>.

What SPG supports (won't be removed in v4.x):

- StartupMessage / R (Auth*) / p (PasswordMessage SASL)
- S (ParameterStatus), K (BackendKeyData), Z (ReadyForQuery)
- Q (simple query) + extended query (P, B, D, E, C, H, S, F)
- T / D / C / E
- d / c / f (COPY)
- X (Terminate)
- SCRAM-SHA-256 auth (RFC 5802)
- pg_catalog subset (pg_class, pg_namespace, pg_database,
  pg_user, pg_tables)

What SPG does not support and won't add in v4.x:

- TLS (`S` startup byte → SPG replies `N`)
- NOTIFY / LISTEN
- Replication protocol (SPG has its own — see `SPG_REPL_ADDR`)
- Cancellation (`CancelRequest`)
- GSSAPI

### SQL surface

Every SQL feature listed in the v3.x corpora (`pgvector`,
`duckdb`, `pg_regress`, `mysql`) and every feature added in
v4.x is part of the stable SQL surface. New features are
additions; existing features keep their semantics within v4.x.

The 4-corpus pass rate is enforced by `cargo run -p sqllogictest
--release` — `prod_ready` row 6.1 (manual eyeball; CI runs it
via the workspace test job).

### Snapshot file format

- Magic: `SPGDB001` (bare catalog) or `SPGENV01` (envelope with
  user table).
- File version range supported: v1 through v8 (current).
- Backwards-compat rule: every v4.x release must load every
  snapshot ever written. `prod_ready` row 8.6 [machine] gates
  this via `tests/cross_version_compat.rs`.

The cross-version test holds the v4.30 snapshot of a known
catalog (`xtests/compat-fixtures/v4.30/`) and asserts the
current binary restores it and produces the same query results.
A future v4.40 would add `compat-fixtures/v4.40/`; the test
walks every fixture directory.

### Backup bundle format

Self-contained file with magic `SPGBKUP\x01` — see
`crates/spg-server/src/backup.rs` doc-comment for the byte
layout. Versioned in-band via the `kind` byte (currently 0 =
full, 1 = incremental); adding new kinds is a MINOR bump,
changing the layout of an existing kind is MAJOR.

### Env-var contract

Every env var listed in DEPLOYMENT.md's table is stable. New
env vars are additions; existing env vars keep their semantics
and acceptable values within v4.x.

Special case: the `SPG_FAIL_*` chaos knobs are explicitly
**not** stable — they exist for testing. Operators must not
depend on them; future versions may rename or remove them
without bumping major.

---

## Not frozen (free to change in any release)

- Internal storage types (`Catalog`, `Table`, `Row`, `Value`)
  — only the on-disk serialization is stable.
- Bench harness output format — `xtests/v4_*.md` reports are
  not API.
- Stderr log message wording — `SPG_LOG_FORMAT=json` gives you
  a structured event stream; the field set is the contract,
  not the text rendering.
- `cargo-target-dir`, `BUDGETS.md`, perf gate budgets — these
  are internal development infrastructure, not user-facing.
- Exact wording in PROD_READY.md / RUNBOOK.md / etc. — the docs
  evolve; the system behaviors they describe stay stable per
  the rules above.

---

## How to add a feature without breaking the contract

1. **New SQL form**: extend the parser, add an `e2e_*` test.
   Free.
2. **New wire opcode**: pick the next free byte, document it in
   the table above, add to `STABILITY.md` and the `prod_ready`
   row 8.2 expected table. Old clients don't send it — fine.
3. **New env var**: document in DEPLOYMENT.md, default-off,
   one-line entry in next CHANGELOG section.
4. **New backup-bundle field**: bump the `kind` byte or add a
   new section *after* every existing field so old parsers stop
   reading at the same place they always did.
5. **New snapshot field**: add to the `FILE_VERSION` integer
   inside `Catalog::serialize` (currently 8 — bump to 9), keep
   the v8 read path. Add a fixture under
   `xtests/compat-fixtures/<this-version>/` so the cross-version
   test gates future regressions.

## How to remove something (you can't, in v4.x)

Anything frozen above can only be removed in a MAJOR bump. If
you need to remove a feature mid-v4.x, deprecate first: emit a
stderr warning when used, document in CHANGELOG, propose the
removal for the v5 milestone.

---

## What this document is NOT

- Not a feature list. See PROD_READY.md.
- Not a tutorial. See DEPLOYMENT.md + RESTORE_DRILL.md.
- Not a roadmap. See RUNBOOK.md / CHANGELOG.md.
