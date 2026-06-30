# Storage format retirement — envelope + segment + identity (v7.37.25.1/2/3/7)

> Documents WHY today's forgiving acceptance of legacy envelope versions
> (V1-V5), segment magic (v1 + v2), and dual identity surfaces
> (auto_increment + IDENTITY) is the right shape for v7.37, and the
> migration script that lands when the retirement is actually safe.

## Where we are today

### Envelope versions (crates/spg-engine/src/envelope.rs)

| Version | Magic + tag      | Body added | Status |
|---------|------------------|------------|--------|
| V1      | `SPGENV01\x01` | catalog + users | accepted on read; never written |
| V2      | `SPGENV01\x02` | + body CRC32 | accepted on read; never written |
| V3      | `SPGENV01\x03` | + publications | accepted on read; never written |
| V4      | `SPGENV01\x04` | + subscriptions | accepted on read; never written |
| V5      | `SPGENV01\x05` | + statistics    | **written by current snapshot path** |

### Segment magic (crates/spg-storage/src/segment.rs)

| Magic         | Status |
|---------------|--------|
| `SEGMENT_MAGIC` (v1) | accepted on read; never written |
| `SEGMENT_MAGIC_V2`   | **written by current freezer path** |

### Identity surface (crates/spg-engine/src/ddl.rs)

| Surface           | Status |
|-------------------|--------|
| `auto_increment`  | MySQL-style column flag; `SetColumnAutoIncrement` path |
| `IDENTITY` (PG)   | `GENERATED { ALWAYS \| BY DEFAULT } AS IDENTITY` lowers to same `SetColumnAutoIncrement` target |
| `SERIAL` (PG)     | parser-level desugars to `column INT DEFAULT nextval(seq)` then back to `SetColumnAutoIncrement` |

The three surfaces converge on one internal `SetColumnAutoIncrement`
target via lowering, so the runtime path is already unified. The
"unify the internal repr" follow-up in 25.7 would rename the variant
to `SetColumnIdentitySequence` (cosmetic) — no behavior change.

## Why retirement is queued, not done

### 25.1 + 25.2: envelope V1/V2/V3 retire

Today's parser accepts all five versions on read. Switching to V4-V5
only would brick any deployment whose snapshot was written by a
pre-V4 server (publications didn't exist before V3; subscriptions
didn't exist before V4).

Required first: a **migration util** that reads V1/V2/V3 and rewrites
as V5. The shape is straightforward (`split_envelope` already
recognises all five; just re-emit via `build_envelope` with the V5
trailer block, leaving the new fields empty). The risk isn't writing
the util — it's running it against every existing deployment before
the parser changes. Customer-impact review queues this against the
v7.38+ release windows because v7.37 has no customer with a
pre-V4 snapshot (mailrs / sentori both came up on v7.32+).

### 25.3: segment magic v1/v2 retire

Same shape as 25.1/2 for segment files. Today's freezer writes V2
exclusively; on read, V1 segments still load. Switching to V2-only
on read is safe in any deployment whose segments were all written
after v6.x (v6.0 is when V2 shipped). Verified across the bundled
mailrs / sentori fixtures (all V2 already). Retirement gates on the
same migration-util shape as envelope, with the same customer-impact
review queue.

### 25.7: auto_increment → IDENTITY internal repr unify

The runtime already converges on `SetColumnAutoIncrement`. 25.7's
remaining work is purely cosmetic — rename the variant +
`SetColumnAutoIncrement` to `SetColumnIdentitySequence` so the AST
spelling matches PG's IDENTITY semantic the column actually carries
in pg_attribute (24.8b-1 `attidentity` column).

The rename touches ~30 sites across `crates/spg-sql/src/ast.rs`,
`crates/spg-sql/src/parser.rs`, `crates/spg-engine/src/ddl.rs`. No
behavior change. Queues alongside parser.rs split (27.4) because
both touch the AST surface and the same git stretch is cheaper than
two passes.

## Migration util shape

Pseudo-code for the envelope retire util when it lands:

```rust
// scripts/spg-envelope-upgrade.rs — bin under spgctl
fn upgrade_envelope(path: &Path) -> std::io::Result<()> {
    let bytes = std::fs::read(path)?;
    let parsed = split_envelope(&bytes);
    let EnvelopeParse::Pair { catalog, users, publications, subscriptions, statistics } = parsed
    else { return Ok(()); }; // already V5 or not an envelope

    // Re-emit as V5 — empty subscriptions / statistics / publications
    // when the input was V1 / V2 / V3.
    let rebuilt = build_envelope(
        catalog,
        users,
        publications.unwrap_or(&[]),
        subscriptions.unwrap_or(&[]),
        statistics.unwrap_or(&[]),
    );
    let tmp = path.with_extension("v5.tmp");
    std::fs::write(&tmp, &rebuilt)?;
    std::fs::rename(tmp, path)
}
```

The actual implementation lands when the parser switches; in the
same commit so the util is the documented escape hatch when a
post-switch deployment hits a pre-V4 snapshot.

## Re-visit signal

- Any customer reports a snapshot that fails `spgctl verify-pitr`
  with "envelope version X not supported" → fix the parser-side
  rejection by running the migration util, then re-emit.
- The five-version surface adds non-trivial mental load to the
  envelope parser — any future envelope work (e.g. v7.40's
  per-schema layout) revisits this and probably folds the
  retire-and-migrate into the same release.

## See also

- `crates/spg-engine/src/envelope.rs` — `split_envelope` /
  `build_envelope` + V1-V5 constants
- `crates/spg-storage/src/segment.rs` — `SEGMENT_MAGIC` /
  `SEGMENT_MAGIC_V2`
- `crates/spg-engine/src/ddl.rs::SetColumnAutoIncrement` — the
  single internal target for SERIAL / IDENTITY / auto_increment
