-- 9.0.2 (C9) — the everyday DDL, with a schema written in front of the name.
--
-- sentori's reply to 9.0.1 found that a written schema was still the search
-- path's to reinterpret outside DML. Widening that question over the
-- statements a migration and `pg_dump` use found the same defect in eleven
-- more places: REFRESH, DROP MATERIALIZED VIEW / VIEW / SEQUENCE / INDEX,
-- CLUSTER, REINDEX, ALTER INDEX and ALTER VIEW each read the name as one
-- token or stripped its schema; ALTER TABLE sw.t RENAME TO moved the table
-- into `public`; and a rename changed the derived constraint names.
SET client_min_messages = warning;
DROP SCHEMA IF EXISTS sw CASCADE;
DROP TABLE IF EXISTS public.t CASCADE;
CREATE SCHEMA sw;
CREATE TABLE public.t (id int PRIMARY KEY, v text);
CREATE TABLE sw.t (id int PRIMARY KEY, v text);
INSERT INTO public.t VALUES (1,'p'); INSERT INTO sw.t VALUES (1,'s');
CREATE VIEW sw.v AS SELECT * FROM sw.t;
CREATE SEQUENCE sw.q;
CREATE INDEX ix ON sw.t (v);
CREATE MATERIALIZED VIEW sw.m AS SELECT id FROM sw.t;
ALTER TABLE sw.t ADD COLUMN w int;
ALTER TABLE sw.t RENAME COLUMN w TO w2;
ALTER INDEX sw.ix RENAME TO ix2;
ALTER SEQUENCE sw.q RESTART WITH 5;
SELECT 'D01'; SELECT nextval('sw.q');
ALTER VIEW sw.v RENAME TO v2;
REFRESH MATERIALIZED VIEW sw.m;
GRANT SELECT ON sw.t TO PUBLIC;
TRUNCATE sw.t;
SELECT count(*) FROM sw.t; SELECT count(*) FROM public.t;
CREATE FUNCTION sw_trg() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RETURN NEW; END $$;
CREATE TRIGGER tr BEFORE INSERT ON sw.t FOR EACH ROW EXECUTE FUNCTION sw_trg();
DROP TRIGGER tr ON sw.t;
ANALYZE sw.t;
VACUUM sw.t;
CLUSTER sw.t USING t_pkey;
REINDEX TABLE sw.t;
ALTER TABLE sw.t RENAME TO t3;
SELECT n.nspname::text||'.'||c.relname::text FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace WHERE n.nspname='sw' ORDER BY 1;
DROP MATERIALIZED VIEW sw.m;
DROP VIEW sw.v2;
DROP SEQUENCE sw.q;
DROP INDEX sw.ix2;
DROP TABLE sw.t3;
SELECT count(*) FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace WHERE n.nspname='sw';
SELECT count(*) FROM public.t;
DROP SCHEMA sw;
DROP TABLE public.t;
DROP FUNCTION sw_trg();
