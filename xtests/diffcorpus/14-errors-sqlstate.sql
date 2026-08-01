SET client_min_messages = warning;
SELECT 'T01'; SELECT 1/0;
SELECT 'T02'; SELECT 'abc'::INT;
SELECT 'T03'; SELECT nosuchfunc(1);
SELECT 'T04'; SELECT * FROM nosuchtable;
SELECT 'T05'; SELECT nosuchcol FROM dc;
SELECT 'T06'; SELECT 1 +;
SELECT 'T07'; INSERT INTO dc (id) VALUES (1);
SELECT 'T08'; SELECT 2147483647::INT * 2;
SELECT 'T09'; SELECT 1.0/0.0;
SELECT 'T10'; SELECT '2020-13-01'::DATE;
SELECT 'T11'; SELECT '{"a"}'::JSONB;
SELECT 'T12'; SELECT ARRAY[1,2] + 1;
SELECT 'T13'; SELECT 'a' AND true;
SELECT 'T14'; SELECT count(*) FROM dc GROUP BY nosuch;
SELECT 'T15'; SELECT id FROM dc HAVING count(*) > 0;
SELECT 'T16'; SELECT id, count(*) FROM dc;
SELECT 'T17'; SELECT * FROM dc a JOIN dc b ON a.nosuch = b.id;
SELECT 'T18'; CREATE TABLE dc (x INT);
SELECT 'T19'; DROP TABLE nosuchtable;
SELECT 'T20'; SELECT (SELECT id FROM dc);
SELECT 'T21'; SELECT ARRAY(SELECT id FROM dc ORDER BY id);
SELECT 'T22'; SELECT '1'::INT[];
SELECT 'T23'; SELECT substring('abc' from 'a(');
SELECT 'T24'; SELECT to_number('x','999');

-- round 625 (S05b/F29)— SPG 曾接受而 PG 拒绝的调用。
-- 立于本轮:131 条这样的形态,语料一条都没照出来(它没问过「该不该报错」)。
-- 只放**两边都必须拒绝**的形态;措辞差异由归一化处理,接受/拒绝的分歧才是要看的。
-- 实参一律显式加类型:SPG 没有 `unknown` 类型,PG 把字面量显示成 `unknown` 而 SPG 显示成 `text`,
-- 那是已记账的呈现差异,不该在这里占住行数、盖住真正要看的判定。
SELECT btrim(1);
SELECT ltrim(1.5);
SELECT rtrim(TRUE);
SELECT lpad(1,5);
SELECT repeat(1,2);
SELECT left(1,1);
SELECT right(1,1);
SELECT translate(1,'a'::TEXT,'b'::TEXT);
SELECT quote_ident(1);
SELECT strpos(1,'a'::TEXT);
SELECT split_part(1,','::TEXT,1);
SELECT replace(1,'a'::TEXT,'b'::TEXT);
SELECT trim(1);
SELECT 1 IS TRUE;
SELECT 1 IS FALSE;
SELECT 1 IS NOT TRUE;
SELECT 1 IS UNKNOWN;
SELECT 'x'::TEXT IS TRUE;
-- 两边都必须接受的对照,免得守卫收过头
SELECT btrim('  x  '), lpad('x',3,'-'), repeat('ab',2), left('abc',2);
SELECT btrim(NULL) IS NULL, TRUE IS TRUE, NULL IS UNKNOWN;
