SET client_min_messages = warning;
SELECT 'T01'; SELECT count(*) > 0 FROM pg_class WHERE relname='dc';
SELECT 'T02'; SELECT attname, atttypid::regtype::text, attnotnull FROM pg_attribute WHERE attrelid='dc'::regclass AND attnum>0 ORDER BY attnum;
SELECT 'T03'; SELECT conname, contype FROM pg_constraint WHERE conrelid='dc'::regclass ORDER BY 1;
SELECT 'T04'; SELECT indexrelid::regclass::text FROM pg_index WHERE indrelid='dc'::regclass ORDER BY 1;
SELECT 'T05'; SELECT table_name, column_name, data_type, is_nullable FROM information_schema.columns WHERE table_name='dc' ORDER BY ordinal_position;
SELECT 'T06'; SELECT table_name FROM information_schema.tables WHERE table_schema='public' AND table_name='dc';
SELECT 'T07'; SELECT constraint_type FROM information_schema.table_constraints WHERE table_name='dc' ORDER BY 1;
SELECT 'T08'; SELECT typname, typtype FROM pg_type WHERE typname IN ('int4','text','numeric','mood','pt') ORDER BY 1;
SELECT 'T09'; SELECT nspname FROM pg_namespace WHERE nspname IN ('public','pg_catalog','information_schema') ORDER BY 1;
SELECT 'T10'; SELECT current_database() IS NOT NULL, current_schema(), version() LIKE 'PostgreSQL%';
SELECT 'T11'; SELECT count(*) > 50 FROM pg_settings;
SELECT 'T12'; SELECT name, setting FROM pg_settings WHERE name IN ('search_path','client_min_messages','DateStyle') ORDER BY 1;
SELECT 'T13'; SELECT count(*) >= 0 FROM pg_stat_activity;
SELECT 'T14'; SELECT proname FROM pg_proc WHERE proname IN ('now','count','upper') ORDER BY 1;
SELECT 'T15'; SELECT count(*) > 0 FROM pg_views WHERE viewname='vv';
SELECT 'T16'; SELECT count(*) > 0 FROM pg_matviews WHERE matviewname='mv';
SELECT 'T17'; SELECT count(*) > 0 FROM pg_tables WHERE tablename='dc';
SELECT 'T18'; SELECT sequence_name FROM information_schema.sequences WHERE sequence_name='sq';
SELECT 'T19'; SELECT count(*) >= 3 FROM information_schema.schemata;
SELECT 'T20'; SELECT has_table_privilege('dc','SELECT'), has_schema_privilege('public','USAGE');
SELECT 'T21'; SELECT obj_description('dc'::regclass) IS NULL;
SELECT 'T22'; SELECT relname, relkind FROM pg_class WHERE relname IN ('dc','vv','mv','sq') ORDER BY 1;
SELECT 'T23'; SELECT count(*) > 0 FROM pg_am; SELECT count(*) > 0 FROM pg_operator; SELECT count(*) > 0 FROM pg_cast;
SELECT 'T24'; SELECT pg_get_constraintdef(oid) FROM pg_constraint WHERE conrelid='dc'::regclass ORDER BY 1;
SELECT 'T25'; SELECT pg_get_indexdef(indexrelid) FROM pg_index WHERE indrelid='dc'::regclass ORDER BY 1;

-- round 623 (S05b)— 目录关系描述自己。
-- 立于本轮:`SELECT count(*) FROM pg_class WHERE relname='pg_class'` 曾答 0(PG 答 1),
-- 而 15 号一条都没照出来 —— 它没问过这个问题。
-- 只放**两边应当逐字节相同**的形态;SPG 合成的目录关系数(22)本就少于 PG(64),
-- 那是覆盖面问题,不放进来当差异。
SELECT count(*) FROM pg_class WHERE relname IN ('pg_class','pg_attribute','pg_type','pg_proc','pg_namespace');
SELECT relname, oid, relkind FROM pg_class WHERE relname IN ('pg_class','pg_attribute','pg_type','pg_proc') ORDER BY relname;
SELECT 'pg_type'::regclass::oid, 'pg_class'::regclass::oid, 'pg_attribute'::regclass::oid;
SELECT n.nspname FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace WHERE c.relname='pg_class';
SELECT attname, attnum FROM pg_attribute a JOIN pg_class c ON c.oid=a.attrelid WHERE c.relname='pg_namespace' ORDER BY attnum;
SELECT count(*) FROM pg_attribute a JOIN pg_class c ON c.oid=a.attrelid WHERE c.relname='pg_namespace';
