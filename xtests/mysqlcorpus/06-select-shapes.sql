-- Query shapes MySQL spells its own way.
SELECT 'T01', id, name FROM mc_t ORDER BY id LIMIT 2;
SELECT 'T02', id, name FROM mc_t ORDER BY id LIMIT 1, 2;
SELECT 'T03', id, name FROM mc_t ORDER BY id LIMIT 2 OFFSET 1;
-- ORDER BY 2, not 1: ordering by the marker literal ties every row
-- and the order is then unspecified on both engines.
SELECT 'T04', id FROM mc_t ORDER BY 2 DESC;
SELECT 'T05', a.id, b.id FROM mc_t a JOIN mc_t b ON a.id = b.id ORDER BY a.id;
SELECT 'T06', a.id, b.id FROM mc_t a LEFT JOIN mc_t b ON a.id = b.id + 10 ORDER BY a.id;
SELECT 'T07', id FROM mc_t WHERE id IN (SELECT id FROM mc_t WHERE qty > 1) ORDER BY id;
SELECT 'T08', EXISTS(SELECT 1 FROM mc_t WHERE qty > 99);
SELECT 'T09', id FROM mc_t UNION SELECT id FROM mc_t ORDER BY 1;
SELECT 'T10', id FROM mc_t UNION ALL SELECT id FROM mc_t ORDER BY 1;
SELECT 'T11', COUNT(*) FROM (SELECT id FROM mc_t) AS d;
SELECT 'T12', name FROM mc_t WHERE name LIKE 'a%' ORDER BY name;
SELECT 'T13', name FROM mc_t WHERE name RLIKE '^[ab]' ORDER BY name;
WITH c AS (SELECT id FROM mc_t WHERE qty > 0) SELECT 'T14', COUNT(*) FROM c;
SELECT 'T15', id, CASE qty WHEN 0 THEN 'zero' WHEN 1 THEN 'one' ELSE 'many' END FROM mc_t ORDER BY id;
SELECT 'T16', DISTINCT_TEST.q FROM (SELECT DISTINCT qty AS q FROM mc_t) AS DISTINCT_TEST ORDER BY 2;
