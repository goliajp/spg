-- port.select_implicit — ported from PG `src/test/regress/sql/select_implicit.sql`.
--
-- Minimal subset: implicit column aliasing + ORDER BY column number.
-- Full port lands during v7.38 P1 fill (target: ≥80% of PG regress
-- select_implicit.sql passing on PG18 oracle).
--
-- Upstream: postgres/postgres `src/test/regress/sql/select_implicit.sql`
-- as of PG18 release. Re-port from upstream when bumping.

-- oracle: depends depd.setup_users

SELECT id, name
FROM users
WHERE org_id = 10
ORDER BY 1;
