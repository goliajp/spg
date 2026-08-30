-- orig.mysql_session_collation — whether `SET NAMES` reaches a bare
-- literal.
--
-- v7.39.3, milestone G4/G8. `SET NAMES utf8mb4 COLLATE utf8mb4_bin`
-- changes how two string LITERALS compare — measured, MySQL 9.7.2 goes
-- from `'AB' = 'ab'` true to false — and SPG carried the session's
-- collation only as far as columns: a query comparing two literals, or
-- ordering them, answered under the default whatever the session had
-- asked for. That is a silent wrong answer rather than an error, and
-- `SET NAMES` is in the first packet of nearly every MySQL client.
--
-- Every row below is a comparison or an ordering, never a collation
-- NAME: the names differ between the two engines, the answers do not.
--
-- Not on the PG leg: `SET NAMES` there sets client encoding, not
-- collation, and PostgreSQL has no session-collation concept to change
-- what two literals compare as.

SELECT 'AB' = 'ab' AS default_ci;
SELECT 'a' < 'B' AS default_order;

SET NAMES utf8mb4 COLLATE utf8mb4_bin;
SELECT 'AB' = 'ab' AS binary_eq;
SELECT 'a' < 'B' AS binary_order;
SELECT 'B' < 'a' AS binary_order_reversed;

-- An explicit COLLATE still overrides the session it is written in.
SELECT 'AB' COLLATE utf8mb4_general_ci = 'ab' AS explicit_wins;

SET NAMES utf8mb4;
SELECT 'AB' = 'ab' AS back_to_ci;
