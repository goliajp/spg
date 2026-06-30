# LSP / IDE setup for SPG

> v7.37.27 (27.9) — audit + reference for working on the SPG
> codebase in an LSP-backed editor (rust-analyzer in any host —
> VS Code, Neovim, Helix, Zed, IntelliJ-RustRover).

## What's intentionally not in the repo

The repo holds **zero** editor-specific config (`.vscode/`,
`.idea/`, `init.lua`-style files) and **zero** rust-analyzer
config (`.cargo/config.toml` per-host overrides,
`rust-analyzer.toml`, `clippy.toml`, `rustfmt.toml`). The
defaults that `cargo fmt --check` and `cargo clippy
--workspace --all-targets -D warnings` enforce in CI are the
defaults rust-analyzer uses out of the box — every contributor's
editor agrees with CI without per-host tuning.

If you want host-side tweaks (font size, keybindings, color
theme), put them in your `$XDG_CONFIG_HOME/<editor>/` — not the
repo.

## The unlinked-file warning

rust-analyzer emits

```
This file is not included anywhere in the module tree, so
rust-analyzer can't offer IDE services.
```

when an integration-test file lands in `tests/e2e/<name>.rs`
before its `mod <name>;` line is added to `tests/e2e/main.rs`.
Two recipes:

- **Add the `mod` line first**, then write the test. The
  warning never appears.
- **Or silence it** — the workspace convention is to add `mod`
  before saving, so the warning is signal, not noise. Disable
  per-host only as a last resort (rust-analyzer setting
  `rust-analyzer.diagnostics.disabled: ["unlinked-file"]`).

## no_std-awareness

Three crates are `#![no_std]` with the `alloc` global allocator
hooked in by the binary crates:

- `spg-storage` — pure no_std.
- `spg-engine` — no_std modulo the `injection-points` feature
  (see [INJECTION_POINTS.md](./INJECTION_POINTS.md)).
- `spg-sql` — pure no_std.

Things this means in the editor:

- **Don't add `use std::…` in these crates.** rust-analyzer will
  surface the import but `cargo check` rejects it. Use
  `alloc::…` instead.
- **Don't reach for `std::collections::HashMap`.** Use
  `alloc::collections::BTreeMap` (deterministic iteration —
  catalog-codec invariant) or the existing FxHashMap re-export
  for hot paths that own their lifetime.
- **`println!` / `eprintln!` don't exist.** For ad-hoc
  debugging, drop a `#[cfg(test)]` block with `std::eprintln!`
  and remove it before commit. Better — use the injection
  points framework (see `crates/spg-engine/src/testkit/
  injection.rs`).

## Feature flags

Standard cargo features at the workspace level:

| Feature              | Crates                  | What it does |
|----------------------|-------------------------|---|
| (default)            | all                     | Production build — every test-only hook is no-op. |
| `injection-points`   | spg-engine              | Activates the deterministic-timing test harness. CI test job builds with this on; production releases off. |

rust-analyzer reads features from `Cargo.toml`'s `[features]`
section + cargo's default-features rule. To exercise the
test-only paths in the editor, set the rust-analyzer cargo
check feature flag list (`rust-analyzer.cargo.features:
["injection-points"]`) per-host.

## Workspace structure (rust-analyzer expectations)

The workspace lives under `Cargo.toml` at the repo root.
rust-analyzer's `linkedProjects` discovers it automatically —
no per-host setup needed.

Sub-crates under `crates/`:

```
crates/
├── spg-engine          # SQL execution core
├── spg-storage         # row + segment storage
├── spg-sql             # SQL parser + AST
├── spg-server          # pgwire / mysqlwire / HTTP server
├── spg-embedded        # in-process embedded API
├── spg-sqlx            # sqlx-compatible driver shim
├── spg-audit           # audit chain
├── spg-crypto          # WAL HMAC / segment encryption
├── spg-wire            # raw wire-protocol primitives
├── spgctl              # CLI
└── …
```

Integration tests live alongside each crate under `tests/`:

- `tests/e2e/` — the merged-per-crate e2e binary (v7.20
  test-speed Part A; ~161 binaries → 1 link in spg-engine).
- `tests/perf_gate/` — release-mode timing budgets.
- `tests/prod_ready/`, `tests/slo_smoke/` — server-side gates.

End-to-end harnesses (sqllogictest, dropin-acceptance) live
under `xtests/`.

## Editor-side cargo cache layout

rust-analyzer reads from cargo's target/ directory just like
the CLI. If the editor's analyze pass races with `cargo test`
in a terminal, both can stall on the file lock. Two recipes:

- Configure rust-analyzer to use a separate target dir
  (`rust-analyzer.cargo.targetDir: true` — defaults to
  `target/rust-analyzer/`).
- Or pin one target dir and live with serialised checks.

For the CI matrix, `scripts/test-on-mini.sh` rsyncs the working
tree to the LAN testbed and runs there; the local target/
stays editor-only. See [TESTING.md](./TESTING.md).

## Clippy + rustfmt convention

CI runs:

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
```

No `rustfmt.toml` / `clippy.toml` overrides — every contributor
sees the same defaults. If a future change adds a workspace-
wide override, it ships in the repo (not the editor).

## What audit-trail expectations look like

The `code-review` skill expects:

- Every commit message names the v7.37.x sub-version the work
  closes (`v7.37.18 (18.x)`).
- Tests + lint + workspace check all pass before commit (the
  branch's commit chain is the audit trail).
- Roadmap update in `.claude/notes/v7.37.x-complete-roadmap.md`
  reflects the closed item.

Each sub-version's autorun batch follows this pattern; the
editor doesn't enforce it but rust-analyzer's inline test-runner
makes the pre-commit cargo-test loop fast.

## Reference

- TESTING.md — five-category test taxonomy.
- INJECTION_POINTS.md — deterministic-timing test harness.
- SPG_TUNABLES.md — env vars catalogue.
- WIRE_FORMAT_PROMISE.md — release-gate enforcement.
