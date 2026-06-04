# spg-embedded-tokio

Tokio-friendly async wrapper around [`spg-embedded`](https://crates.io/crates/spg-embedded).

## Why this exists

`spg-embedded::Database::execute(&mut self, sql)` is sync. Calling it
from inside `tokio::main` blocks a worker thread until the engine
returns — which, on the file-backed path, can take an fsync.
`spg-embedded-tokio` wraps that sync handle in a
`tokio::sync::Mutex<Database>` and dispatches every call through
`tokio::task::spawn_blocking`. The Mutex matches the engine's
**single-writer invariant**; `spawn_blocking` insulates the
runtime's worker pool from disk stalls.

## Quick start

```toml
[dependencies]
spg-embedded-tokio = "7.10"
tokio = { version = "1", features = ["rt-multi-thread", "macros"] }
```

```rust
use spg_embedded_tokio::{AsyncDatabase, Value};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let db = AsyncDatabase::open_path("./mydb.spg").await?;

    db.execute("CREATE TABLE IF NOT EXISTS messages (
        id BIGSERIAL PRIMARY KEY,
        body TEXT NOT NULL,
        received_at TIMESTAMPTZ NOT NULL DEFAULT now()
    )").await?;

    db.execute("INSERT INTO messages (body) VALUES ('hello tokio')").await?;

    let rows = db.query("SELECT id, body FROM messages ORDER BY id DESC LIMIT 1").await?;
    for row in rows {
        let Value::BigInt(id) = row[0] else { unreachable!() };
        let Value::Text(ref body) = row[1] else { unreachable!() };
        println!("{id}: {body}");
    }
    Ok(())
}
```

## API

| Method | Notes |
| --- | --- |
| `AsyncDatabase::open_in_memory()` | Cheap; no disk I/O. |
| `AsyncDatabase::open_path(path).await` | File-backed; replays WAL on open. |
| `db.execute(sql).await` | DDL / DML. Returns `QueryResult`. |
| `db.query(sql).await` | SELECT. Returns `Vec<Vec<Value>>`. |
| `db.checkpoint().await` | Flush WAL into snapshot + truncate. |

`AsyncDatabase` is `Clone` — every clone shares the same underlying
engine and lock, like `Arc<Mutex<…>>` would.

## Concurrency model

The single-writer invariant is **not** about thread safety —
it's a correctness guarantee from SPG's design. There is at
most one in-flight engine call at any moment, even under
concurrent callers. The Tokio `Mutex` enforces it.

## Fan-out reads — `AsyncReadHandle`

Read-heavy workloads (mailrs IMAP fetch, scan-heavy analytics)
should NOT serialise on the same `Mutex` as writes. Use
`AsyncReadHandle` to take a snapshot-isolated read view:

```rust
let h = db.read_handle().await;
let rows = h.query("SELECT * FROM messages WHERE folder = 'inbox'").await?;
```

Multiple `AsyncReadHandle`s run concurrently — they don't acquire
the writer lock at query time. The snapshot is frozen at the
moment `read_handle()` returns; subsequent writes are NOT
visible. Call `handle.refresh().await` to re-snapshot when you
need fresher data:

```rust
let mut h = db.read_handle().await;
loop {
    let rows = h.query("SELECT * FROM messages").await?;
    if rows_changed(&rows) { break; }
    h.refresh().await;
}
```

Snapshots are cheap (catalog is backed by `PersistentVec`; clone
is O(log n) per table). Take one per concurrent reader.

DDL / DML through a read handle returns
`EngineError::WriteRequired` — go through `db.execute()` instead.

## Versioning

`spg-embedded-tokio` versions track the SPG workspace. Use the
same major.minor as the rest of the SPG crates you depend on
(7.10.x with this crate; 7.10.x of `spg-embedded`).

## License

MIT OR Apache-2.0 — same as the rest of the SPG workspace.
