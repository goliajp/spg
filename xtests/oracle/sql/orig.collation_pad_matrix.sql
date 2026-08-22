-- orig.collation_pad_matrix — the pad attribute is a property of the
-- collation NAME, and both engines agree on the mapping.
--
-- v7.38.17. The oracle servers are pinned to `utf8mb4_bin` on purpose
-- (see mysql/my.cnf: it mirrors PG's C locale so one fixture can run on
-- three legs), which means every fixture before this one was blind to
-- collation behaviour. This one declares its collations, the way a
-- customer's dump does, so the pin cannot hide them.
--
-- Measured on MySQL 9.7.2 and MariaDB 12.3.2 — `information_schema`
-- reports the same pad attribute for each name on both:
--
--     utf8mb4_0900_ai_ci     NO PAD
--     utf8mb4_bin            PAD SPACE
--     utf8mb4_general_ci     PAD SPACE
--
-- so over rows 'alpha', 'alpha  ', 'Beta', 'beta':
--
--     collation             COUNT(DISTINCT)   WHERE s = 'alpha'
--     utf8mb4_0900_ai_ci           3                 1
--     utf8mb4_bin                  3                 2
--     utf8mb4_general_ci           2                 2
--
-- SPG answers the first row correctly as of v7.38.17 and the other two
-- wrongly (4/1 and 3/1): it reads a collation name for whether to FOLD
-- and never for whether to PAD. That gap is locked in `.spg.out` rather
-- than left out of the corpus, so it is visible every run and deleting
-- the lock is what closing it looks like.

CREATE TABLE pad_0900 (s VARCHAR(32) COLLATE utf8mb4_0900_ai_ci);
CREATE TABLE pad_bin (s VARCHAR(32) COLLATE utf8mb4_bin);
CREATE TABLE pad_gen (s VARCHAR(32) COLLATE utf8mb4_general_ci);

INSERT INTO pad_0900 VALUES ('alpha'),('alpha  '),('Beta'),('beta');
INSERT INTO pad_bin VALUES ('alpha'),('alpha  '),('Beta'),('beta');
INSERT INTO pad_gen VALUES ('alpha'),('alpha  '),('Beta'),('beta');

SELECT COUNT(DISTINCT s) FROM pad_0900;
SELECT COUNT(*) FROM pad_0900 WHERE s = 'alpha';
SELECT COUNT(DISTINCT s) FROM pad_bin;
SELECT COUNT(*) FROM pad_bin WHERE s = 'alpha';
SELECT COUNT(DISTINCT s) FROM pad_gen;
SELECT COUNT(*) FROM pad_gen WHERE s = 'alpha';
