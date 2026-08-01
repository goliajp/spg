-- 19 — SQLSTATE 码本身,不比消息文本。
--
-- 立于 round 622。缘由:本轮实测发现 SPG 对**整个「参数类型不对」面**都答
-- 类码 `42000`(而不是 PG 的 42883 / 42846 / 22023),13 个形态中招 —— 而
-- 18-sqlstate 一条都没照出来。原因是既有语料比的是**完整输出**,SPG 与 PG
-- 的措辞本就不同(`upper() needs text, got integer` vs
-- `function upper(integer) does not exist`),那一行无论错码对不对都算「差异」,
-- 于是错码问题永远藏在措辞差异后面。
--
-- `\set VERBOSITY sqlstate` 让 psql **只印错码**,两边就能干净对比。措辞对齐
-- 是另一件事(SPG 不建重载表,不打算伪造 `is not unique` 这种理由),这份文件
-- 刻意不管它。
--
-- 只放**两边都必须拒绝**的形态:一边接受一边拒绝是覆盖面问题,归 14 号。
\set VERBOSITY sqlstate

-- 函数实参类型不对 —— PG 一律 42883(没有候选签名匹配这次调用)
SELECT upper(1);
SELECT lower(ARRAY[1,2]);
SELECT abs('x'::TEXT);
SELECT length(TRUE);
SELECT round('x'::TEXT);
SELECT to_char(ARRAY[1], 'x');
SELECT date_trunc('day', 1);
SELECT extract(hour FROM 1);
SELECT array_length(1, 1);
SELECT unnest(1);
SELECT jsonb_each(1::INT);

-- 聚合实参类型不对 —— 同样 42883
SELECT sum('x'::TEXT);
SELECT bool_and(1);

-- 运算符没有候选 —— 同样 42883
SELECT 1 LIKE 2;
SELECT 'a'::TEXT + 1;
SELECT (ARRAY[1,2])[1] = 'x'::TEXT;
SELECT ARRAY[1,2] @> 1;

-- 强转无路径 —— PG 42846 CANNOT_COERCE
SELECT ARRAY[1]::INT;
SELECT TRUE::TIMESTAMP;
SELECT 1::INET;

-- jsonb 值变不成目标类型 —— PG 22023
SELECT '{"a":1}'::JSONB::INT;
SELECT jsonb_array_length('{"a":1}'::JSONB);

-- 文本表示不合法 —— PG 22P02 / 22007(与上面三类分开,别被误路由)
SELECT 'x'::INT;
SELECT 'x'::NUMERIC;
SELECT 'x'::UUID;
SELECT 'x'::DATE;

\set VERBOSITY default
