-- Errors: the message, the errno and the SQLSTATE are all part of the
-- MySQL face a driver reads.
SELECT 'T01' FROM no_such_table_here;
SELECT 'T02', no_such_column FROM mc_t;
SELECT 'T03', 1/0, 1 DIV 0, MOD(1,0);
INSERT INTO mc_t (id, name) VALUES (1, 'dup-pk');
INSERT INTO mc_t (name, qty) VALUES ('overflow', 999);
CREATE TABLE mc_t (x INT);
DROP TABLE no_such_table_here;
SELECT 'T04', CAST('notanumber' AS SIGNED);
SELECT 'T05', CAST('2020-99-99' AS DATE);
INSERT INTO mc_t (name, made) VALUES ('baddate', '2020-99-99');
SELECT * FROM mc_t GROUP BY name;
SELECT 'T06' WHERE;
UPDATE mc_t SET nope = 1;
SELECT 'T07', COUNT(*) FROM mc_t;
