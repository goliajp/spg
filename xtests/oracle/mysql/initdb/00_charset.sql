-- Charset on the default DB created from MYSQL_DATABASE. Image
-- already runs CREATE DATABASE with utf8mb4 default; the ALTER
-- below is a belt-and-braces pin so the runner can rely on
-- collation invariants.
ALTER DATABASE testdb CHARACTER SET = utf8mb4 COLLATE = utf8mb4_bin;
