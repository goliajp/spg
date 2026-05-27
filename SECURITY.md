# SPG security

## Reporting a vulnerability

If you find a security issue in SPG, please **do not** open a
public GitHub issue. Instead:

- Email: `security@<your-domain>` (replace with your actual
  contact; this is a template).
- Or: open a private security advisory via GitHub's
  "Security → Report a vulnerability" tab.

Maintainer commitment:

- Acknowledge within **2 business days**.
- Initial triage within **7 days** (severity assignment, expected
  fix window).
- Coordinated disclosure: 90-day default, faster for actively
  exploited issues, longer by mutual agreement.

## What's in scope

- Memory safety in spg-* crates (workspace lint:
  `unsafe_code = "deny"`; only `spg-crypto` has a local
  `allow(unsafe_code)` for SIMD intrinsics).
- Authentication / authorization correctness: native AUTH +
  PG-wire SCRAM-SHA-256 + RBAC role enforcement.
- WAL / snapshot tampering detection at restore time.
- SQL / wire-frame parser robustness (no crashes on adversarial
  input).
- Audit log integrity (BLAKE3 hash chain).
- Replication protocol over trusted network — see "Trust model"
  below.

## What's out of scope (by design)

- **TLS / wire encryption**: SPG never offered TLS at the
  protocol level. Run behind `stunnel`, `nginx`, `pgbouncer`, or
  the equivalent. The wire protocol is binary, length-prefixed,
  and predictable; any TLS-terminating proxy works.
- **Side-channel attacks**: timing-leaks on SCRAM, BLAKE3, or
  password verification are out of scope; SPG doesn't run untrusted
  code in the same process.
- **Multi-tenant isolation**: SPG runs as a single tenant per
  process. Run separate `spg-server` processes per tenant if you
  need isolation.
- **Replication over untrusted networks**: the replication
  protocol has no authentication — the follower handshake is the
  string `b"SPGREPL\x01"` and a starting offset. Put primary and
  followers on a trusted VPC / VPN.
- **Denial of service**: rate-limiting is on the operator
  (`SPG_MAX_CONNECTIONS`, `SPG_QUERY_TIMEOUT_MS`,
  `SPG_MAX_QUERY_ROWS`). SPG doesn't ship a per-IP rate limiter.

## Secret handling

- The bootstrap admin password is read from `SPG_ADMIN_PASSWORD`
  env var on first run only. Subsequent runs ignore it (admin
  is now persistent in the user store).
- `SPG_PASSWORD` (legacy single-password mode) is also env-only.
  Do not put it in CLI args — `ps` reveals them.
- Salts are per-user 16 random bytes from `/dev/urandom` at
  user-creation time. Each password hash is
  `BLAKE3(salt || password)`; the SCRAM secret is stored
  alongside per RFC 5802.
- The audit log records SQL text verbatim (intentional). If
  your queries embed secrets, **rotate** before turning audit
  on, or set `audit_path = -` to disable.

## Dependency hygiene

- `cargo-audit` runs in CI on every PR
  (`.github/workflows/ci.yml::cargo-audit` job). An advisory
  match fails the build.
- `cargo-deny` is tracked for v4.31 — will add license allowlist
  + duplicate-version checks.
- Workspace `spg-*` crates have zero external Rust deps; only
  `xbench/competitor/` (offline benchmark harness) pulls sqlx
  and tokio. Production binaries don't link them.
- `rust-toolchain.toml` pins to 1.95.0; CI matches.

## Threat model: what an attacker with each kind of access can do

| attacker has                       | can do                                                                 | mitigation                                |
|------------------------------------|------------------------------------------------------------------------|-------------------------------------------|
| network reachability to `SPG_ADDR` | submit SQL (rejected without AUTH if password / RBAC configured)       | always set `SPG_ADMIN_PASSWORD`           |
| valid readonly creds               | read any table they're SELECTed against                                | RBAC + per-deployment table partitioning  |
| valid readwrite creds              | INSERT/UPDATE/DELETE any table they have access to                     | RBAC `admin` is required for user mgmt    |
| read access to the db / wal files  | recover everything (no at-rest encryption)                             | filesystem-level encryption (LUKS, FileVault) |
| write access to the db / wal files | tamper with state silently; tamper with audit log is **detected**      | filesystem ACLs; audit log mandatory      |
| read access to the audit file      | learn the history of every successful DDL/DML SQL string               | filesystem ACLs                           |
| network reachability to `SPG_REPL_ADDR` | become a fake follower, receive full snapshot + every future WAL | put repl on private network or VPN        |

## Known limitations (not vulnerabilities, but documented gaps)

- **In-memory phantom rows after WAL append failure**: see
  PROD_READY row 1.11. A failed WAL append leaves the engine's
  in-memory state with the row; restart fixes it. v4.30
  preflight closes this for the chaos knob; real ENOSPC mid-
  `write_all` retains a small window. Tracked for post-v4.32.
- **No per-query memory cap**: PROD_READY row 5.5. A malicious
  authenticated user could submit a query whose intermediate
  state allocates more than your free memory. Mitigate with
  `SPG_MAX_QUERY_ROWS` and OS-level cgroup limits.

## Disclosure log

(None yet. Future entries: date, CVE if assigned, severity,
fixed-in version, summary.)
