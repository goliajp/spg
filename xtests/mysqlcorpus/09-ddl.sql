-- MySQL DDL spellings: AUTO_INCREMENT, ENGINE, CHARSET, ON UPDATE.
DROP TABLE IF EXISTS mc_e;
CREATE TABLE mc_e (
  id BIGINT UNSIGNED NOT NULL AUTO_INCREMENT PRIMARY KEY,
  s  VARCHAR(16) CHARACTER SET utf8mb4 COLLATE utf8mb4_general_ci NOT NULL,
  n  DECIMAL(6,3) UNSIGNED DEFAULT NULL,
  y  YEAR DEFAULT NULL,
  ts TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP,
  e  ENUM('a','b','c') DEFAULT 'a',
  st SET('x','y') DEFAULT NULL,
  UNIQUE KEY u_s (s),
  KEY k_n (n)
) ENGINE=InnoDB AUTO_INCREMENT=100 DEFAULT CHARSET=utf8mb4 COMMENT='corpus';
SELECT 'T01', COUNT(*) FROM mc_e;
INSERT INTO mc_e (s,n,y,e,st) VALUES ('one',1.500,2020,'b','x,y');
SELECT 'T02', id, s, n, y, e, st FROM mc_e;
ALTER TABLE mc_e ADD COLUMN extra INT NULL AFTER s;
SELECT 'T03', COUNT(*) FROM mc_e WHERE extra IS NULL;
ALTER TABLE mc_e MODIFY COLUMN extra BIGINT NULL;
ALTER TABLE mc_e CHANGE COLUMN extra extra2 BIGINT NULL;
ALTER TABLE mc_e DROP COLUMN extra2;
ALTER TABLE mc_e ADD INDEX k_y (y);
ALTER TABLE mc_e DROP INDEX k_y;
ALTER TABLE mc_e RENAME TO mc_e2;
SELECT 'T04', COUNT(*) FROM mc_e2;
RENAME TABLE mc_e2 TO mc_e;
SELECT 'T05', COUNT(*) FROM mc_e;
-- `CREATE TABLE … LIKE` is a syntax error on SPG (ledgered), and the
-- two statements after it were only ever reporting that. Kept as one
-- line so the difference stays visible without cascading.
CREATE TABLE mc_like LIKE mc_e;
DROP TABLE IF EXISTS mc_like;
DROP TABLE mc_e;
