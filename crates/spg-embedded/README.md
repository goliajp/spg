# spg-embedded

Embedded SQL database for Rust. Single-writer, WAL-backed,
zero external dependencies (no `proc-macro2`, no `syn`, no
`tokio`, no `rusqlite`). PG-wire compatible when paired with
[`spg-server`](https://crates.io/crates/spg-server).

## Throughput

Numbers from `cargo bench -p spg-embedded` on
Apple M-series silicon. Reproduce with
`crates/spg-embedded/benches/embedded.rs`.

| Operation                              | Time/op   | Ops/sec    |
|----------------------------------------|-----------|------------|
| INSERT (in-memory)                     | ~0.6 µs   | ~1.7 M     |
| INSERT (persistent, one fsync per row) | ~4 ms     | ~250       |
| SELECT by PK (BTree seek)              | ~1.7 µs   | ~600 k     |
| Vector kNN, k=10, dim=8 (HNSW)         | ~1.9 µs   | ~520 k     |

Persistent INSERT is the worst case — one `fsync` per call.
For bulk loads, wrap many INSERTs in
`db.with_transaction(|tx| { … })`: the WAL fsyncs once at
COMMIT instead of per statement, and throughput approaches
the in-memory number.

```rust
use spg_embedded::Database;

let mut db = Database::open_path("/data/app.db")?;
db.execute("CREATE TABLE users (id INT NOT NULL, name TEXT)")?;
db.execute("INSERT INTO users VALUES (1, 'alice')")?;
let rows = db.query("SELECT name FROM users WHERE id = 1")?;
# Ok::<(), spg_embedded::EngineError>(())
```

## Why

- **Zero external dependencies.** The whole stack (parser,
  planner, executor, storage, WAL) lives inside the workspace.
  No transitive `serde` blow-up, no proc-macro chains, no
  build-time async runtime. Your `Cargo.lock` stays small.
- **Crash-safe by default.** Every committed mutation `fsync`s
  before the call returns. Boot replays the WAL. Catalog
  snapshots use atomic rename. Tested under simulated
  half-write and torn-record failure modes.
- **Single-writer model.** No MVCC, no deadlocks, no lost
  updates. Concurrent reads share an `Arc<Mutex<Database>>`.
  Writes serialize, but a single writer on modern hardware
  is faster than you'd expect.
- **PG-flavoured SQL.** Real `JOIN`s, real `FOREIGN KEY ...
  ON DELETE/UPDATE`, real `CHECK`-less but `NOT NULL` /
  `DEFAULT` / `AUTO_INCREMENT`, real pgvector-style
  `VECTOR(N)` with HNSW / SQ8 / HALF. 4-corpus regression at
  100% pass (`pg_regress` 144/144, `pgvector` 63/63,
  `mysql` 100%, `duckdb` 100%).
- **First-class vector search.** Built-in HNSW with `<->`
  (L2), `<#>` (inner product), `<=>` (cosine) operators. No
  separate extension to install.

## When to use

| Use case | spg-embedded fit |
|---|---|
| Replacing SQLite in a Rust binary | ✅ Cleaner API, vectors built-in, FK enforcement |
| Per-tenant local DB in a multi-tenant service | ✅ One file per tenant, snapshot to backup |
| Edge / desktop app with cold-tier data | ✅ Hot/cold tier moves data to disk automatically |
| ML inference with vector recall | ✅ HNSW + pgvector ops without an extension |
| OLAP / analytics over moderate datasets | ✅ Designed for it |
| Multi-master OLTP | ❌ Single-writer (axiom A1) |
| Triggers / stored procs / RLS | ❌ See [`PG_MIGRATION.md`](../../PG_MIGRATION.md) |
| Sub-millisecond latency at QPS > 100k | Probably need `spg-server` + connection pool |

## Quick start (persistent)

```rust,no_run
use spg_embedded::Database;

fn main() -> Result<(), spg_embedded::EngineError> {
    // Opens an existing DB or creates one on first run.
    let mut db = Database::open_path("/data/app.db")?;

    db.execute("CREATE TABLE IF NOT EXISTS users (
        id INT NOT NULL,
        name TEXT,
        joined TIMESTAMP DEFAULT NOW()
    )")?;
    db.execute("CREATE INDEX IF NOT EXISTS users_pk ON users (id)")?;

    db.execute("INSERT INTO users (id, name) VALUES (1, 'alice')")?;

    // Each successful execute() fsyncs the WAL before returning.
    Ok(())
}
```

## Typed queries

The `spg_row!` declarative macro generates a `FromSpgRow`
impl for a plain Rust struct. No proc-macros, no
`#[derive(...)]`.

```rust,no_run
use spg_embedded::{Database, spg_row};

spg_row! {
    pub struct User {
        pub id: i32,
        pub name: Option<String>,
    }
}

let mut db = Database::open_in_memory();
db.execute("CREATE TABLE u (id INT NOT NULL, name TEXT)").unwrap();
db.execute("INSERT INTO u VALUES (1, 'alice'), (2, NULL)").unwrap();

let users: Vec<User> = db.query_typed("SELECT id, name FROM u").unwrap();
for u in &users {
    println!("{}: {:?}", u.id, u.name);
}
```

## Transactions

```rust,no_run
use spg_embedded::Database;

let mut db = Database::open_path("/data/app.db").unwrap();
db.with_transaction(|tx| {
    tx.execute("INSERT INTO ledger (acct, delta) VALUES (1, -100)")?;
    tx.execute("INSERT INTO ledger (acct, delta) VALUES (2, 100)")?;
    Ok::<_, spg_embedded::EngineError>(())
}).unwrap();
```

The closure runs inside a `BEGIN` / `COMMIT` pair. Returning
`Err` from the closure triggers `ROLLBACK` automatically. The
WAL fsyncs once at `COMMIT`, not per statement — bulk loads
should go through this path.

## Foreign keys (v7.6+)

```rust,no_run
# use spg_embedded::Database;
# let mut db = Database::open_in_memory();
db.execute("CREATE TABLE u (id INT NOT NULL)").unwrap();
db.execute("CREATE INDEX u_pk ON u (id)").unwrap();
db.execute("CREATE TABLE o (
    id INT NOT NULL,
    uid INT NOT NULL REFERENCES u(id) ON DELETE CASCADE
)").unwrap();
```

Full surface: `[CONSTRAINT name] FOREIGN KEY (cols)
REFERENCES tbl[(pcols)] [ON DELETE …] [ON UPDATE …]` with
actions `CASCADE | RESTRICT | SET NULL | SET DEFAULT |
NO ACTION`. Composite (multi-column) and self-referencing
FKs supported. `ALTER TABLE … ADD/DROP CONSTRAINT …`
supported.

## Vectors

```rust,no_run
# use spg_embedded::Database;
let mut db = Database::open_in_memory();
db.execute("CREATE TABLE docs (
    id INT NOT NULL,
    emb VECTOR(128) NOT NULL
)").unwrap();
db.execute("CREATE INDEX docs_emb ON docs USING hnsw (emb)").unwrap();

// Nearest-neighbour query — pgvector-style.
let rows = db.query("
    SELECT id FROM docs
    ORDER BY emb <-> [0.1, 0.2, /* … */ 0.128]
    LIMIT 10
").unwrap();
```

## Concurrency

`Database` is `Send` but **not** `Sync`. Share across threads
with `Arc<Mutex<Database>>`. The single-writer architecture
is the design (axiom A1) — see
[`STABILITY.md`](../../STABILITY.md).

## Background freezer (optional)

```rust,no_run
use spg_embedded::{Database, FreezerOptions, spawn_background_freezer};
use std::sync::{Arc, Mutex};
use std::time::Duration;

let db = Arc::new(Mutex::new(Database::open_path("/data/app.db").unwrap()));
let freezer = spawn_background_freezer(
    db.clone(),
    FreezerOptions {
        tick: Duration::from_secs(30),
        hot_tier_bytes: 64 * 1024 * 1024,
        batch_rows: 10_000,
        ..Default::default()
    },
);

// ... do work ...

freezer.stop();
```

The freezer moves the oldest hot rows into compressed cold
segments on disk. Reads transparently span both tiers. Cold
segments persist across restarts via the catalog manifest.

## Panic contract

- `execute()` / `query()` calls never panic on user input.
  Malformed SQL, type mismatches, missing tables all return
  `Err(EngineError::…)`.
- The library panics only on internal invariant violations
  (catalog snapshot magic mismatch, WAL record CRC sentinel
  corruption that survived boot-time validation). These
  represent silent disk corruption.
- Release profile uses `panic = abort` — host dies fast on
  poisoned state. Build with `--profile release-dbg` for
  unwind tables + `catch_unwind`.

## License

MIT OR Apache-2.0
