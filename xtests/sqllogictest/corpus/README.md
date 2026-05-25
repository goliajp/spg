# SPG conformance corpus

Hand-authored sqllogictest probes, organised by what they would conform
against in the real world:

- **`pgvector/`** — vector type, `<->`, ORDER BY distance LIMIT. Mirrors the
  feature surface of the pgvector extension (PG 18 + pgvector ≥ 0.7).
- **`duckdb/`** — PG-dialect SELECT / DML / DDL. DuckDB is the closest
  PG-flavour test corpus we can target without psql-format wrangling.
- **`pg_regress/`** — features from PG's `src/test/regress/sql/` that
  individual `.sql` files exercise: CREATE TABLE shape, INSERT INTO, SELECT
  shape, DML the v1 doesn't implement.

These are *authored*, not copied — to skip the license dance, and because
copying real upstream files would force translating psql output dialects.
The baseline tells us how SPG measures against the *concept* of each corpus.

Pass rate categories:

| Corpus | Probe count | P-tier | Target |
|---|---|---|---|
| pgvector | ~50 records | P0 | 100% |
| duckdb | ~80 records | P1 | ≥ 60% |
| pg_regress | ~30 records | P2 | ≥ 30% |
