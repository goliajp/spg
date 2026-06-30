# WAL sync invariants (v7.37.25.8)

> The dual-handle (`wal` + `wal_sync_clone`) WAL write path's correctness
> invariants in one place. Source files cite this doc rather than
> re-deriving each invariant inline.

## The dual handle

`ServerState` holds two `File` references to the same WAL inode:

- `wal: Option<Arc<Mutex<File>>>` — mutex-guarded write handle. All byte
  appends go through this.
- `wal_sync_clone: Option<Arc<File>>` — `try_clone`'d handle pointing at
  the same kernel inode. Only used for `sync_data()`.

Both opened at `open_wal_for_append` (`crates/spg-server/src/main.rs`).
The clone happens once at startup; failure to clone leaves
`wal_sync_clone = None` and the fsync path falls back to taking the
mutex briefly.

## Why two handles

The single-handle alternative — fsync under the mutex — serializes
client INSERTs behind the flusher thread's `sync_data` (~5 ms on macOS
APFS, ~0.5 ms on Linux ext4 NVMe). v5.4.4's "async-commit 9× slower
than sync" regression came from exactly this: the flusher monopolised
the mutex during fsync, INSERTs queued, throughput collapsed.

The dual handle lets the write path and the fsync path operate on the
same kernel file concurrently. The OS sees both as the same inode;
`sync_data` issues `fsync(fd)` on the clone's fd, the kernel flushes
the shared inode's dirty pages, the data is durable when `sync_data`
returns. The mutex stays free for client writes.

## Invariants the source code relies on

1. **Same inode.** `wal_sync_clone` is produced by `File::try_clone()`
   on the open `wal` File before any writes. Both `File`s wrap fds that
   point at the same kernel inode (POSIX `dup` semantics; equivalent on
   Windows via `DuplicateHandle`). Cannot be replaced by `File::open()`
   on the same path — that would race with `wal_path` rename during
   chunk rotation.

2. **Writes precede fsyncs.** The `append_durability_marker` path
   writes the marker bytes under the mutex, drops the mutex, then calls
   `sync_data` on the clone. The fsync covers every byte written **up to
   and including** the marker because the inode-level write is
   already kernel-visible by the time the mutex is dropped (no buffered
   userspace state between `wal.write_all` and the implicit fd flush at
   drop-time of the borrow).

3. **No partial-write window.** `wal.write_all(&entry)` is atomic with
   respect to other appenders because of the mutex. It is not atomic
   with respect to the fsync — but that's the point: the fsync just
   needs the bytes to be in the kernel before it returns, which
   `write_all` guarantees.

4. **The clone fd cannot be closed before the mutex fd.** Both are
   wrapped in `Arc`; either Drop runs only when the last `Arc` clone
   is gone. Server shutdown drops `ServerState` last, which drops both
   handles together.

5. **Fallback is correct, not fast.** When `try_clone` fails at startup
   (e.g. process fd table exhausted), the fsync path acquires the mutex
   to call `sync_data` directly. This restores serialised correctness
   at the cost of v5.4.2's async-commit throughput. Logged at startup so
   the degradation is observable.

6. **Chunk rotation respects both handles.** When the WAL rolls over to
   a new chunk file, BOTH the mutex-guarded `wal` and the
   `wal_sync_clone` are re-bound to the new file before the next write
   lands. Rotation is documented at `crates/spg-embedded/src/lib.rs`'s
   chunk-rotation site; the server-side equivalent is in
   `open_wal_for_append`.

## Why this is the simplest correct shape

Alternatives considered and rejected:

- **Single `RwLock<File>` with read-guard for sync.** `sync_data` takes
  `&File`, so `&self`-borrowing the inner File via read-guard works in
  principle. But the rotation path needs `&mut File` to swap the inode,
  which requires the write-guard, which conflicts with concurrent
  read-guarded syncs in a way that the dual-Arc design avoids by giving
  rotation its own grace period.

- **Fully lock-free using `IoSlice` + `pwritev`.** Eliminates the mutex
  entirely. But the durability-marker write path needs to atomically
  see the pre-marker offset AND write the marker bytes — the
  read-then-write would race with concurrent appenders. The mutex is
  the cheapest way to get that atomicity portably.

- **Dropping the durability marker entirely.** The marker is what lets
  recovery skip uncommitted v3 records cleanly. Eliminating it would
  collapse the dual-handle but lose the recovery shape; the trade-off
  isn't favorable.

## When to revisit

- If a customer perf gate hits the marker-write path as a bottleneck:
  re-measure under `samply` to confirm the mutex hold (microseconds) is
  actually the limit, not the fsync (milliseconds). If the mutex is
  truly the limit, consider amortising the marker across N records
  (group commit).
- If the underlying File abstraction changes (e.g. `io_uring`-backed
  open replaces `std::fs::File`): the `try_clone` shape may need
  rewording; check that the new abstraction preserves the
  inode-equivalence semantic.

## See also

- `crates/spg-server/src/wal.rs::append_durability_marker` — the path
  this doc explains
- `crates/spg-server/src/main.rs::open_wal_for_append` — where the
  clone happens
- `crates/spg-embedded/src/lib.rs::wal_sync_data` — the embedded
  counterpart (single-handle because no flusher thread races there)
