-- orig.mysql_introducers — the charset introducer, which `orig.mysql_
-- family_literals` had to leave out.
--
-- v7.39.3. When that fixture was written SPG could not run an
-- introducer at all — `SELECT _utf8mb4'x'` answered `ERROR 1064 syntax
-- error at or near "'x'"` while both oracles answered `x` — and a
-- statement the parser refuses aborts the whole leg, so it was recorded
-- as an open gap (milestone G3) rather than written as three lines that
-- make the file unrunnable. It runs now, so it is a fixture.
--
-- What an introducer does is fix the CHARACTER SET of the literal that
-- follows, which decides how it compares. `_binary` is the one with
-- observable consequences on every engine: it makes the comparison
-- byte-for-byte, so `'AB' = 'ab'` goes from true to false without any
-- session setting changing.
--
-- COLLATION() of an introduced literal is NOT here: MySQL 9.7.2 answers
-- `utf8mb3_general_ci` for `N'y'` and the two engines name their default
-- collations differently, which this corpus does not normalise. The
-- VALUES and the COMPARISONS are what all three agree on.
--
-- Not on the PG leg: PostgreSQL has no introducer syntax.

SELECT _utf8mb4'x' AS utf8mb4_intro;
SELECT N'y' AS n_intro;
SELECT _binary'z' AS binary_intro;
SELECT _latin1'w' AS latin1_intro;

-- The consequence, not the spelling: the same pair of literals compares
-- equal under the session's collation and unequal as bytes.
SELECT 'AB' = 'ab' AS session_collation;
SELECT _binary'AB' = _binary'ab' AS as_bytes;

-- An introducer takes an explicit COLLATE, and it wins over the session.
SELECT _utf8mb4'AB' COLLATE utf8mb4_bin = _utf8mb4'ab' AS introduced_then_collated;
