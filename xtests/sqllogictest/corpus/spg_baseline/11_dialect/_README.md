# 11 — Dialect

PG-specific (`::`, `||`, RETURNING, ILIKE, jsonb ops) plus MySQL
(`ON DUPLICATE KEY UPDATE`, `STRAIGHT_JOIN`) plus MariaDB
(`INSERT IGNORE`, CREATE SEQUENCE) — anything that's not standard SQL
but SPG accepts for drop-in compat.
