-- The SHOW family, which is MySQL's catalog surface.
DROP TABLE IF EXISTS mc_i;
CREATE TABLE mc_i (a INT NOT NULL, b VARCHAR(32) NOT NULL, c INT, PRIMARY KEY (a,b), KEY kb (b(4)), KEY kc (c));
SHOW INDEX FROM mc_i;
SHOW COLUMNS FROM mc_i;
SHOW CREATE TABLE mc_i;
DROP TABLE IF EXISTS mc_fk;
CREATE TABLE mc_fk (x INT NOT NULL, y VARCHAR(32) NOT NULL, z INT, UNIQUE KEY uq (x,y), KEY kz (z), FOREIGN KEY (x,y) REFERENCES mc_i(a,b));
SHOW CREATE TABLE mc_fk;
DROP TABLE mc_fk;
SELECT 'T01', COUNT(*) FROM information_schema.statistics WHERE table_name='mc_i';
SELECT 'T02', column_name, data_type, is_nullable, column_key
  FROM information_schema.columns WHERE table_name='mc_i' ORDER BY ordinal_position;
SELECT 'T03', index_name, seq_in_index, column_name, non_unique
  FROM information_schema.statistics WHERE table_name='mc_i' ORDER BY index_name, seq_in_index;
SELECT 'T04', constraint_name, constraint_type
  FROM information_schema.table_constraints WHERE table_name='mc_i' ORDER BY constraint_name;
ALTER TABLE mc_i DROP INDEX kc;
SHOW INDEX FROM mc_i;
DROP INDEX kb ON mc_i;
SELECT 'T05', COUNT(*) FROM information_schema.statistics WHERE table_name='mc_i';
DROP TABLE mc_i;
