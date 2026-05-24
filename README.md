# SPG — Small / Smart PostgreSQL

A research / open-source single-database RDS written in **pure Rust 2024**, with
**zero runtime dependencies** (std-only) and PG SQL-dialect compatibility.

Designed to be embedded inside a Docker image as the lightweight RDS for one
project — perf / mem / footprint are first-class goals.

## Status

Pre-alpha. v0.1 ships only the project skeleton and the self-built wire-frame
PING/PONG round-trip. SQL, storage, and pgvector arrive in later milestones.

## Build

```sh
cargo build --workspace
cargo test  --workspace
```

## Constraints

- **Business code is `std`-only** (`forbid(unsafe_code)` workspace-wide). All
  infrastructure libraries — wire protocol, storage engine, B-tree, HNSW, WAL,
  crypto hash, etc. — are written from scratch. Third-party crates are allowed
  only in `dev-dependencies` (tests, benches, build scripts).
- **Self-built client tooling**: a custom CLI talks to the daemon over a
  self-defined wire protocol. SPG does **not** speak the PostgreSQL wire
  protocol. The **SQL dialect**, however, mirrors PostgreSQL so that existing
  PG queries are portable.
- Targets the financial-industry deployment niche: append-only audit log with
  cryptographic hash-chain (self-built BLAKE3) is part of v1.

## License

MIT OR Apache-2.0
