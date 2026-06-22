# 16_isolation — 轴 4 isolation / 并发 corpus (in-progress)

> **Status**: gap-tracking placeholder. v7.38 plan §二 轴 4 calls
> for a Rust isolationtester + PG `src/test/isolation/specs/*.spec`
> vendoring + Hermitage 11 case × 3 iso level grid + Elle + Jepsen
> postgresql-12.3 reverse test. None of that has landed.
>
> **What this dir holds today**:
> - Gap tests that document the SURFACES SPG owes per the vision
>   "全面 ≥ PG" contract.
> - Marked as `statement error` for surfaces SPG doesn't yet
>   accept; flipped to `statement ok` + behavioural assertions
>   when the surface lands.
>
> This mirrors the oracle's EXPECTED FAILURE convention — failure
> is locked in so a future commit can't silently change the answer
> without the test author noticing.

## Index

| File | Surface | Status |
|---|---|---|
| `set_transaction_isolation_level.test` | `SET TRANSACTION ISOLATION LEVEL { READ COMMITTED \| REPEATABLE READ \| SERIALIZABLE }` + `START TRANSACTION ISOLATION LEVEL …` + `SHOW transaction_isolation` | ⚠️ gap — parse fails today |

## How to flip a gap to LANDED

1. Implement the parser + engine state for the surface.
2. Change `statement error` → `statement ok` for each previously-
   gap-tracking line in the .test file.
3. Add behavioural assertions (e.g. `SHOW transaction_isolation`
   returns the value set).
4. Update the Status column in this README.
5. The commit message references which v7.38 plan section §
   ascended.
