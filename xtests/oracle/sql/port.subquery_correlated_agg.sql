-- port.subquery_correlated_agg — outer-aggregate correlated subquery.
--
-- Minimal reproducer for backlog R32: a correlated subquery whose
-- outer reference appears inside an aggregate function. PG handles
-- this via the standard outer-ref lift; SPG currently mis-resolves
-- the column and either errors or returns the wrong rows.
--
-- This fixture is **anchored as EXPECTED FAILURE** via the
-- companion `expected/port.subquery_correlated_agg.spg.out` lock —
-- see that file's header for the divergence note and the runner
-- semantics. When R32 closes, the runner reports the lock as stale
-- and demands its deletion; the test then enforces the PG18
-- baseline directly.

-- oracle: depends depd.setup_users

SELECT u.id,
       u.name,
       (SELECT COUNT(*)
        FROM users v
        WHERE v.org_id = u.org_id) AS org_size
FROM users u
ORDER BY u.id;
