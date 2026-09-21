-- 9.0.0 (C9) — a relation in a schema of its own, on every catalog
-- surface that names one.
--
-- 9.0.0 shipped with half of this: the relations were separate and most
-- of the surfaces still said `public`, or said nothing at all. Six of
-- them were wrong at once, and each was found by comparing THIS file's
-- output against PostgreSQL rather than by any test in the repository —
-- an index's namespace, a view's schema in two views, a view's columns,
-- `pg_indexes`, and `'sa.t'::regclass`.
--
-- Two of them also put the stored KEY into text a client reads, which
-- truncates at the separator: a view's stored body became
-- `SELECT id FROM "sa` and could not be re-parsed at all.
SET client_min_messages = warning;
DROP SCHEMA IF EXISTS sa CASCADE;
DROP SCHEMA IF EXISTS sb CASCADE;
DROP TABLE IF EXISTS pub_t CASCADE;
CREATE SCHEMA sa;
CREATE SCHEMA sb;
CREATE TABLE pub_t(id int primary key, n text);
CREATE TABLE sa.t(id int primary key, n text);
CREATE TABLE sb.t(id int primary key, n text);
CREATE INDEX sa_t_n ON sa.t(n);
CREATE VIEW sa.v AS SELECT id FROM sa.t;
CREATE SEQUENCE sa.s;
INSERT INTO sa.t VALUES (1,'a');
INSERT INTO sb.t VALUES (2,'b');
SELECT 'S01'; SELECT n.nspname, c.relname, c.relkind FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace WHERE n.nspname IN ('sa','sb','public') AND c.relname IN ('t','v','s','sa_t_n','pub_t') ORDER BY 1,2;
SELECT 'S02'; SELECT table_schema, table_name, table_type FROM information_schema.tables WHERE table_schema IN ('sa','sb') ORDER BY 1,2;
SELECT 'S03'; SELECT table_schema, table_name, column_name FROM information_schema.columns WHERE table_schema IN ('sa','sb') ORDER BY 1,2,3;
SELECT 'S04'; SELECT schemaname, tablename FROM pg_tables WHERE schemaname IN ('sa','sb') ORDER BY 1,2;
SELECT 'S05'; SELECT schemaname, indexname, tablename FROM pg_indexes WHERE schemaname IN ('sa','sb') ORDER BY 1,2;
SELECT 'S06'; SELECT schemaname, viewname FROM pg_views WHERE schemaname IN ('sa','sb') ORDER BY 1,2;
SELECT 'S07'; SELECT schemaname, sequencename FROM pg_sequences WHERE schemaname IN ('sa','sb') ORDER BY 1,2;
SELECT 'S08'; SELECT constraint_schema, table_name, constraint_type FROM information_schema.table_constraints WHERE constraint_schema IN ('sa','sb') ORDER BY 1,2,3;
SELECT 'S09'; SELECT 'sa.t'::regclass::text, 'sb.t'::regclass::text;
SELECT 'S10'; SELECT count(*) FROM sa.t; SELECT 'S11'; SELECT count(*) FROM sb.t;
SELECT 'S12'; SELECT nspname FROM pg_namespace WHERE nspname IN ('sa','sb') ORDER BY 1;
SELECT 'S13'; SELECT schema_name FROM information_schema.schemata WHERE schema_name IN ('sa','sb') ORDER BY 1;
SELECT 'S14'; SELECT indexdef FROM pg_indexes WHERE indexname='sa_t_n';
SELECT 'S15'; SELECT id FROM sa.v ORDER BY 1;
SELECT 'S16'; SELECT table_schema, table_name FROM information_schema.views WHERE table_schema='sa';
SELECT 'S17'; SELECT count(*) FROM pg_attribute a JOIN pg_class c ON c.oid=a.attrelid JOIN pg_namespace n ON n.oid=c.relnamespace WHERE n.nspname='sa' AND c.relname='t' AND a.attnum>0;
DROP SCHEMA sa CASCADE;
DROP SCHEMA sb CASCADE;
DROP TABLE pub_t;
