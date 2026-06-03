# Publishing SPG crates to crates.io

This is the user-driven publish playbook. The Claude session
can prepare metadata, run dry-runs, and verify the dependency
graph — but the actual `cargo publish` step needs the user's
crates.io token and is irreversible, so the human pushes the
button.

## Pre-flight

1. Workspace builds clean:
   ```
   cargo build --workspace --release
   cargo test --workspace --release
   ```
2. All crates have non-`path`-only deps (path + version)
   declared. crates.io strips `path =`; only `version = …`
   matters for the published manifest.
3. `cargo login <token>` once per shell (token from
   https://crates.io/me).

## Dependency-ordered publish list

Each crate must be published **after** every crate it depends
on, in this exact order:

| # | Crate          | Depends on (internal)             |
|---|----------------|-----------------------------------|
| 1 | `spg-wire`     | (none)                            |
| 2 | `spg-crypto`   | (none)                            |
| 3 | `spg-sql`      | (none)                            |
| 4 | `spg-storage`  | crypto                            |
| 5 | `spg-audit`    | crypto                            |
| 6 | `spg-manifest` | crypto                            |
| 7 | `spg-engine`   | crypto, sql, storage              |
| 8 | `spg-embedded` | crypto, storage, manifest, engine |
| 9 | `spg-cli`      | wire, storage, engine, crypto     |
| 10| `spg-server`   | wire, sql, storage, audit, engine, crypto, manifest |

## Dry-run every crate

```
for c in spg-wire spg-crypto spg-sql spg-storage spg-audit \
         spg-manifest spg-engine spg-embedded spg-cli spg-server; do
  echo "=== $c ==="
  cargo publish -p "$c" --dry-run --allow-dirty
done
```

`--allow-dirty` only if you have local edits you don't intend
to commit (e.g. workspace version bumped but un-committed).
Otherwise commit first and run without it.

## Publish, in order

After the workspace tag (`git tag -a v7.7.0 …`) is pushed:

```
for c in spg-wire spg-crypto spg-sql spg-storage spg-audit \
         spg-manifest spg-engine spg-embedded spg-cli spg-server; do
  echo "=== publishing $c ==="
  cargo publish -p "$c"
  # crates.io needs a moment to index a new version before the
  # next crate can resolve it transitively. 30 s is usually
  # enough; 60 s is safer.
  sleep 30
done
```

If any one fails with "crate version 7.7.0 already exists",
that crate was already published (e.g. partial earlier
publish). Skip it manually and resume.

## After publish

- Verify each crate lands on crates.io with the right
  description / categories / keywords.
- Update README / homepage links if the canonical doc has
  moved.
- Tag the release on GitHub if not done yet.

## A note on path + version deps

This workspace currently declares internal deps as
`spg-foo = { path = "../spg-foo" }`. Before publish, every
such line needs a `version` too:

```
spg-foo = { path = "../spg-foo", version = "7.7" }
```

`cargo publish` rejects path-only deps; the `version =` is
what ends up in the published manifest. This sweep is the
last step before the first publish run; once every dep has a
version, future releases just need the workspace version bump
to flow through.
