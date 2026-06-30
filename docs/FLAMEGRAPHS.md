# Flame graphs in SPG

SPG does not ship a built-in flame-graph export endpoint, because the
native Rust tooling already does the job better than a re-implementation
behind a custom catalog view would:

| Need | Tool | Where it runs |
|------|------|---------------|
| CPU flame graph of a live query | `samply record -- spg-server` then visit `http://127.0.0.1:3000` | dev / staging — single binary, no agent |
| Allocations-by-call-path | `samply --mode allocation` (macOS) or `heaptrack` (Linux) | dev |
| Per-template hot list (no graph, fast lookup) | `pg_catalog.pg_stat_statements` `total_exec_time` / `mean_exec_time` / `max_exec_time` columns | prod — always-on, no profiler attach |
| Per-template row stats | `pg_catalog.pg_stat_statements.rows` | prod |
| Top-N watcher (live tail) | `spg top --interval 2 --limit 10` (v7.37.22.10) | prod |
| Per-query EXPLAIN ANALYZE call tree | `EXPLAIN (ANALYZE, BUFFERS, TIMING)` | dev / prod |

The combination of `pg_stat_statements` (where time is spent across
templates) + `samply` (where time is spent inside one template's call
tree) covers everything a custom flame-graph endpoint would expose,
without forcing SPG to host a sampling profiler in-process. `spg top`
makes the picking of "which template to profile next" a 2-second
operation against a running server.

This is the same shape as the PG community's recommended stack
(pg_stat_statements + linux perf + FlameGraph.pl) — we just substitute
samply for perf since SPG ships on macOS as a first-class target and
samply renders interactive flame graphs without an extra render step.

## Recipes

### "Which query template is eating CPU?"

```sh
spg top --interval 2 --limit 15
```

### "Where inside that template is the time going?"

```sh
# In one terminal — start SPG under samply.
samply record -- ./target/release/spg-server --listen 127.0.0.1:25432

# In another — re-run the offending workload (or wait for it to
# recur naturally).

# Stop the workload, then Ctrl-C the samply terminal. Samply opens
# the profile viewer in a browser at the URL it prints.
```

### "Is this allocator pressure?"

```sh
# macOS — samply 0.13+
samply record --mode allocation -- ./target/release/spg-server

# Linux — heaptrack
heaptrack ./target/release/spg-server
heaptrack_gui heaptrack.spg-server.*.zst
```

### "Production — can't attach a profiler, what do I have?"

`pg_stat_statements` + `spg top` together. The
`total_exec_time` / `max_exec_time` / `rows` columns suffice to rank
candidates; the EXPLAIN cost numbers tell you which executor stage
dominates each candidate. When a candidate looks structural (always
the same hot row count + always the same plan + still slow), reproduce
it on a dev replica and switch to `samply`.

## Why no `spg flamegraph` subcommand

A native subcommand would have to either:

1. **Embed a sampling profiler in the server process** — adds always-on
   sampling overhead even when nobody is looking, and ships profiler
   state machine + symbol resolution that samply already does.
2. **Spawn samply / perf under the hood** — a 6-character wrapper around
   `samply record -- spg-server` that hides which tool is being used,
   making it harder to debug when the profiler itself misbehaves.

Neither earns its keep against the existing pipeline. If a future
customer requirement materially changes this calculus (e.g. need for
continuous always-on flame graphs in production), revisit and add the
endpoint then. Until then, document the native workflow honestly so
operators reach for the right tool first.

## See also

- [SPG_TUNABLES.md](./SPG_TUNABLES.md) — `SPG_*` env vars including the
  query-stats budget knobs that bound `pg_stat_statements` memory.
- [PERF_METHODOLOGY_VS_FOSS.md](./PERF_METHODOLOGY_VS_FOSS.md) — the
  decomposition-vs-polish methodology that `samply` feeds.
- `pg_catalog.pg_stat_statements` view (v7.37.22.1 — 38 PG-canonical
  columns).
- `spg top` subcommand (v7.37.22.10) — top-N live watcher built on
  pg_stat_statements.
