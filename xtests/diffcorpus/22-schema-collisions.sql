-- 9.0.1 (C9) — one NAME in two schemas, which is where a catalog that
-- resolves a relation by the name a client reads answers for the wrong
-- one.
--
-- 21-schemas.sql gives each schema its own names, so every surface in
-- it can be right while an oid is shared. Under THIS shape 9.0.0
-- published two `pg_class` rows carrying one oid, answered
-- `pg_get_sequence_data` for only one of two sequences — and
-- PostgreSQL's own `pg_dump`, which joins the two, SEGFAULTED (rc=139).
--
-- The primary key's name is the clearest view of the same cause: it
-- was built from the stored KEY, and a key reaches a client as a C
-- string that stops at the key's own separator, so `sa\0t_pkey`
-- arrived as `sa` — and restoring that dump failed with
-- `constraint "sa" for relation "t" already exists`.
SET client_min_messages = warning;
DROP SCHEMA IF EXISTS ca CASCADE;
DROP TABLE IF EXISTS t CASCADE;
DROP VIEW IF EXISTS v CASCADE;
DROP SEQUENCE IF EXISTS s CASCADE;
CREATE SCHEMA ca;
CREATE TABLE t(id int primary key, a text);
CREATE TABLE ca.t(id int primary key, b text);
INSERT INTO t VALUES (1,'pub');
INSERT INTO ca.t VALUES (9,'ca');
CREATE VIEW v AS SELECT id FROM t;
CREATE VIEW ca.v AS SELECT id FROM ca.t;
CREATE SEQUENCE s START 5;
CREATE SEQUENCE ca.s START 50;
CREATE INDEX ti ON t(a);
CREATE INDEX tj ON ca.t(b);
-- K01 — no oid names two relations. `pg_dump` EXECUTEs one prepared
-- statement per object; a shared oid stops the dump outright.
SELECT 'K01'; SELECT count(*) FROM (SELECT oid FROM pg_class GROUP BY oid HAVING count(*) > 1) d;
-- K02 — the floor for K01: the relations are all there to collide.
SELECT 'K02'; SELECT n.nspname, c.relname, c.relkind, c.relpersistence FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace WHERE n.nspname IN ('ca','public') AND c.relname IN ('t','v','s','ti','tj','t_pkey') ORDER BY 1,2,3;
-- K03 — each sequence answers under its OWN oid with its own value.
-- This join is `pg_dump`'s; a sequence with no row here is the NULL it
-- dereferenced.
SELECT 'K03'; SELECT c.relname, n.nspname, d.last_value FROM pg_catalog.pg_sequence q JOIN pg_class c ON c.oid = q.seqrelid JOIN pg_namespace n ON n.oid = c.relnamespace, pg_get_sequence_data(q.seqrelid) d WHERE n.nspname IN ('ca','public') ORDER BY 2,1;
-- K04 — a sequence in a schema advances by its own name.
SELECT 'K04'; SELECT nextval('ca.s'), nextval('s'), currval('ca.s');
-- K05 — a constraint is named after the BARE table and lives in the
-- table's own schema.
SELECT 'K05'; SELECT conname, conrelid::regclass::text, contype FROM pg_constraint WHERE conrelid::regclass::text IN ('t','ca.t') ORDER BY 2,1,3;
-- K06 — a view's body reaches a client whole.
SELECT 'K06'; SELECT pg_get_viewdef('ca.v'::regclass);
-- K07 — the qualifier is written exactly when a client would need one.
SELECT 'K07'; SET search_path = public; SELECT 'ca.t'::regclass::text, 'public.t'::regclass::text;
SELECT 'K08'; SET search_path = ca, public; SELECT 'ca.t'::regclass::text, 'public.t'::regclass::text;
-- K09 — …and from the oid side, which is how a catalog join renders one.
SELECT 'K09'; SELECT c.oid::regclass::text FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace WHERE c.relname = 't' AND c.relkind = 'r' AND n.nspname IN ('ca','public') ORDER BY 1;
-- K10 — a written qualifier is not the search path's to reinterpret.
SELECT 'K10'; SELECT a FROM public.t ORDER BY 1;
SELECT 'K11'; INSERT INTO public.t VALUES (3,'ins'); SELECT count(*) FROM public.t; SELECT count(*) FROM ca.t;
SELECT 'K12'; UPDATE public.t SET a = 'x' WHERE id = 1; SELECT a FROM public.t WHERE id = 1; SELECT b FROM ca.t;
SELECT 'K13'; DELETE FROM public.t WHERE id = 3; SELECT count(*) FROM public.t;
-- K14 — the floor for K10..K13: an UNqualified name still follows the
-- path, which is the rule the qualifier is an exception to.
SELECT 'K14'; SELECT b FROM t;
SELECT 'K15'; SELECT to_regclass('ca.t')::text, to_regclass('ca.nope') IS NULL;
RESET search_path;
DROP SCHEMA ca CASCADE;
DROP TABLE t CASCADE;
DROP VIEW IF EXISTS v CASCADE;
DROP SEQUENCE IF EXISTS s CASCADE;
