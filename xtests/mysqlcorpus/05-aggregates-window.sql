-- Aggregates, GROUP_CONCAT (MySQL's own), and window functions.
SELECT 'T01', COUNT(*), COUNT(amt), COUNT(DISTINCT qty) FROM mc_t;
SELECT 'T02', MIN(qty), MAX(qty), SUM(qty), AVG(qty) FROM mc_t;
SELECT 'T03', GROUP_CONCAT(name ORDER BY id) FROM mc_t;
SELECT 'T04', GROUP_CONCAT(name ORDER BY name SEPARATOR '|') FROM mc_t;
SELECT 'T05', GROUP_CONCAT(DISTINCT qty ORDER BY qty DESC) FROM mc_t;
SELECT 'T06', qty, COUNT(*) FROM mc_t GROUP BY qty ORDER BY qty;
SELECT 'T07', qty, COUNT(*) FROM mc_t GROUP BY qty HAVING COUNT(*) >= 1 ORDER BY qty;
-- ORDER BY 2, not 1: ordering by the marker literal ties every row and
-- the order is then unspecified on both engines — MySQL falls back to
-- the rollup's own key order and SPG keeps the group order, and neither
-- is wrong.
SELECT 'T08', SUM(qty) FROM mc_t GROUP BY qty WITH ROLLUP ORDER BY 2;
SELECT 'T09', id, ROW_NUMBER() OVER (ORDER BY id) FROM mc_t ORDER BY id;
SELECT 'T10', id, RANK() OVER (ORDER BY qty), DENSE_RANK() OVER (ORDER BY qty) FROM mc_t ORDER BY id;
SELECT 'T11', id, LAG(qty) OVER (ORDER BY id), LEAD(qty) OVER (ORDER BY id) FROM mc_t ORDER BY id;
SELECT 'T12', id, SUM(qty) OVER (ORDER BY id ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW) FROM mc_t ORDER BY id;
SELECT 'T13', BIT_AND(qty), BIT_OR(qty), BIT_XOR(qty) FROM mc_t;
SELECT 'T14', STD(qty), STDDEV_SAMP(qty), VARIANCE(qty) FROM mc_t;
SELECT 'T15', ANY_VALUE(name) IS NOT NULL FROM mc_t;
