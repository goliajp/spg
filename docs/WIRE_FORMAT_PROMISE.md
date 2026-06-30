# SPG Wire Format Promise

> v7.37.25 (25.6) — written commitment to the wire-protocol
> compatibility tiers SPG ships and supports.

SPG speaks three wire-level dialects:

1. PostgreSQL wire protocol (pgwire) — **first-class**
2. MySQL wire protocol (mysqlwire) — **best-effort**
3. SPG admin protocol over HTTP — **internal**

Each tier carries a different correctness contract. This
document is the canonical reference for what "supported" means
at each level. Customer integrations that read this document
can size their own test matrix and SLOs accordingly.

## Tier 1: PostgreSQL wire — first-class

PostgreSQL is SPG's lead wire and the model against which the
engine's catalog and SQL semantics are designed. The contract:

- **Hard fan-in:** any pgwire-compatible client that works against
  PostgreSQL 18.x against the supported feature set in
  [PG_DROPIN.md](./PG_DROPIN.md) works against SPG without a
  customer-side code change. Same connection string, same
  prepared-statement API, same wire types.
- **Same handshake:** SPG advertises a `PostgreSQL 18.0 (SPG
  v7.37.x)` server_version_string and the PG-compatible
  ParameterStatus packets every client expects.
- **Same extended protocol:** Parse / Bind / Describe / Execute /
  Sync follow PG-canonical message shapes. Portals are stateful
  and reusable across Execute calls within a transaction. SPG's
  prepared-statement cache invalidates on the same events PG
  invalidates on (DDL touching the statement's source tables;
  ANALYZE bumping the stats version).
- **Same error codes:** SQLSTATE codes match PG one-to-one for
  every shape SPG produces. Customer error-routing logic
  (`if state == '23505' { ... }`) works without an SPG-specific
  branch.
- **Same dump round-trip:** `pg_dump | psql` against SPG must
  produce a logically-equivalent catalog. The release.sh gate
  runs this dump→load→dump diff against 10 PG production
  schemas; any drift fails the release.
- **Same regression suite:** PG 18's `src/test/regress/sql/*.sql`
  is in the release gate at the percentages each sub-version's
  shape supports. Drift between SPG and PG on a previously-passing
  case is a P0 cherry-pick.
- **Same dashboards:** every PG monitoring tool reads SPG's
  `pg_stat_statements`, `pg_stat_activity`, `pg_locks`,
  `pg_class`, `pg_attribute`, `pg_index`, `pg_constraint`,
  `pg_proc`, `pg_type`, `pg_enum`, `information_schema.*`
  surfaces. Column names + types + row shapes match PG; columns
  not yet wired by the engine return shape-stable defaults so
  the dashboards still parse. v7.37.24 closed this audit.

## Tier 2: MySQL wire — best-effort

mysqlwire is for migrations off MariaDB / MySQL where the
customer wants to avoid changing their drivers. The contract:

- **Drop-in for the dump+driver path:** `mysqldump` against SPG
  is the supported migration shape. SPG accepts what's needed
  to load mysqldump output (`SET FOREIGN_KEY_CHECKS = 0;`,
  `LOCK TABLES`, `UNLOCK TABLES`, `/*!40000 …*/` versioned
  conditional blocks).
- **Drop-in for sqlx-mysql:** the sqlx-mysql driver's handshake
  + extended-protocol path resolves correctly. Customer apps
  that compile against sqlx-mysql today migrate by changing
  the connection URL, no other change.
- **Not all MySQL features work:** spatial types, `FULLTEXT`
  indices, `STORED` generated columns, MyISAM-specific syntax,
  `INSERT ... ON DUPLICATE KEY UPDATE` with deferred resolution
  all fall outside the supported set. `MYSQL_DROPIN.md` and
  `MARIADB_DROPIN.md` are the authoritative lists.
- **Catalog shape may differ:** `information_schema.STATISTICS`
  exposes the SPG-internal view; `mysql.user` is synthesised for
  the bootstrap-compat probe shape only.

## Tier 3: SPG admin (HTTP) — internal

The HTTP admin interface is SPG's own wire — there is no
upstream protocol to mirror. It exposes:

- `/health` — liveness probe, returns 200 when the engine
  responds.
- `/metrics` — Prometheus-shaped metrics scrape.
- `/spg/audit_verify` — audit-chain integrity check.

Compatibility guarantee: only that breaking changes ship with
a major-version bump (the SPG semver-major). No promises about
PG / MySQL parallels here.

## Why this matters

Without a written promise, customer integration teams spend
their first week against SPG figuring out which features
they're allowed to depend on. With the promise, they look at
this document, see "first-class for pgwire" + the linked
[PG_DROPIN.md](./PG_DROPIN.md) feature matrix, and start
shipping. The promise also gives the SPG team a backstop when
prioritising follow-up work: "this would break Tier 1" is a
P0 release blocker; "this would degrade Tier 2 by one feature"
is a P3 follow-up.

## Test gates

The PG-first promise is enforced by these gates (all in
`release.sh`):

1. **G1 — workspace tests:** `cargo test --workspace` must be
   green. Catches catalog drift before the dump/regression
   pass.
2. **G2 — sqllogictest:** entire `xtests/sqllogictest/corpus`
   green. Catches wire-format drift end-to-end.
3. **G3 — mailrs dogfood:** real production schema +
   workload replays cleanly. Catches subtle behavior drift
   that schema-only tests miss.
4. **G4 — dump-compat:** pg_dump round-trip diff = 0 against
   10 production schemas.
5. **G5 — data-compat:** SPG-produced dump loads against
   PostgreSQL 18.

Any G1–G5 failure halts the release. A PG-first guarantee with
no enforcement gate is just an aspiration; the gates are how
the promise becomes a contract.

## Related

- [PG_DROPIN.md](./PG_DROPIN.md) — supported PG feature matrix.
- [MYSQL_DROPIN.md](./MYSQL_DROPIN.md) — supported MySQL
  feature matrix.
- [MARIADB_DROPIN.md](./MARIADB_DROPIN.md) — supported MariaDB
  feature matrix.
- [TESTING.md](./TESTING.md) — five-category test taxonomy that
  G1–G5 ride on top of.
