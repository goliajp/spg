-- 9.0.1 — the ORDER BY surfaces that were not asking the collation.
--
-- Every row below turns on ONE pair of names, `tj` and `t_pkey`, which
-- the two orders disagree about: bytes put `t_pkey` first (`_` is 0x5F,
-- `j` is 0x6A) and an `en_US.utf8` locale puts `tj` first (it ignores
-- the punctuation). The pair is deliberate — it is the pair a catalog
-- listing produces on its own, from a table `t` with an index.
--
-- Four things were wrong at once, and each one is one row here:
--
--   * `name` carries the `C` collation in PostgreSQL whatever the
--     database collates as, and SPG compared it under the database's —
--     so every `ORDER BY relname` came back in a different order;
--   * a UNION's combined sort was collation-blind;
--   * so were the four synthetic-source sorts (VALUES, unnest,
--     generate_series, jsonb_each_text / derived table);
--   * and a whole-row key's Array comparison dropped the collation, so
--     `ORDER BY <row>` compared its text field by bytes.
--
-- The same rows read from a TABLE were right throughout, which is what
-- kept all four out of sight.
SET client_min_messages = warning;
DROP TABLE IF EXISTS nc CASCADE;
CREATE TABLE nc(x text);
INSERT INTO nc VALUES ('tj'),('t_pkey');
-- N01 — the control: from a table, which was always right.
SELECT 'N01'; SELECT x FROM nc ORDER BY x;
-- N02 — `name` is `C`, and `text` is the database's, in one row.
SELECT 'N02'; SELECT 'tj'::name < 't_pkey'::name, 'tj'::text < 't_pkey'::text;
-- N03 — a `name` takes the comparison with it whatever the other side is.
SELECT 'N03'; SELECT 'tj'::name < 't_pkey', 'tj'::name < 't_pkey'::text;
-- N04 — an explicit collation still wins over the type's own.
SELECT 'N04'; SELECT x FROM (VALUES ('tj'::name),('t_pkey'::name)) v(x) ORDER BY x COLLATE "en_US.utf8";
-- N05 — …and without one, the type's `C`.
SELECT 'N05'; SELECT x FROM (VALUES ('tj'::name),('t_pkey'::name)) v(x) ORDER BY x;
-- N06 — a catalog listing, which is what N02 is FOR.
SELECT 'N06'; CREATE INDEX tj ON nc(x); SELECT relname FROM pg_class WHERE relname IN ('tj','nc') ORDER BY relname;
-- N07 — a UNION's combined sort.
SELECT 'N07'; SELECT x FROM nc UNION ALL SELECT x FROM nc ORDER BY x;
-- N08 — a VALUES list as a FROM item.
SELECT 'N08'; SELECT x FROM (VALUES ('tj'::text),('t_pkey'::text)) v(x) ORDER BY x;
-- N09 — a set-returning function's rows.
SELECT 'N09'; SELECT u FROM unnest(ARRAY['tj','t_pkey']) u ORDER BY u;
-- N10 — a whole-row key: PostgreSQL compares a record field by field,
-- each field under its own collation.
SELECT 'N10'; SELECT nc FROM nc ORDER BY nc;
SELECT 'N11'; SELECT nc FROM nc ORDER BY nc DESC;
-- N12 — an array key, the shape a record's key is built as.
SELECT 'N12'; SELECT ARRAY[x] FROM nc ORDER BY 1;
-- N13 — min / max over an EXPRESSION. Only a BARE COLUMN took a
-- collation here, so `min(x || '')` compared by bytes while `min(x)`
-- over the same column compared under the database's.
SELECT 'N13'; SELECT min(x), min(x||''), min(upper(x)) FROM nc;
-- N14 — where a collation COMES FROM: the inputs, carried through a
-- cast; the result type's own applies only when no input has one.
-- `min(x::name)` over a text column is the locale's order, a `name`
-- COLUMN is `C`, and two `name` LITERALS are `C`.
DROP TABLE IF EXISTS nn CASCADE;
CREATE TABLE nn(x name);
INSERT INTO nn VALUES ('tj'),('t_pkey');
SELECT 'N14'; SELECT min(x::name) FROM nc;
SELECT 'N15'; SELECT min(x), max(x) FROM nn;
SELECT 'N16'; SELECT x FROM nn ORDER BY x;
SELECT 'N17'; SELECT x FROM nc ORDER BY x::name;
DROP TABLE nn CASCADE;
DROP TABLE nc CASCADE;
