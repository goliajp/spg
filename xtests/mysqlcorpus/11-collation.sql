-- Charset and collation: the surface where SPG must claim MySQL 8.0's
-- defaults, not MariaDB's, and where PAD SPACE lives.
SELECT 'T01', @@character_set_server, @@collation_server;
SELECT 'T02', @@character_set_connection, @@collation_connection;
SELECT 'T03', 'a' = 'A' COLLATE utf8mb4_general_ci;
SELECT 'T04', 'a' = 'A' COLLATE utf8mb4_bin;
SELECT 'T05', 'a ' = 'a' COLLATE utf8mb4_general_ci;
SELECT 'T06', 'a ' = 'a' COLLATE utf8mb4_0900_ai_ci;
SELECT 'T07', 'a ' = 'a' COLLATE utf8mb4_bin;
-- One function per statement: a missing one takes its whole row with
-- it, and COERCIBILITY is missing while the other two may not be.
SELECT 'T08', COLLATION('x'), CHARSET('x');
SELECT 'T08b', COERCIBILITY('x');
SELECT 'T09', HEX(CONVERT('日' USING utf8mb4)), HEX(CONVERT('a' USING binary));
SELECT 'T10', name FROM mc_t ORDER BY name COLLATE utf8mb4_bin, id;
SELECT 'T11', name FROM mc_t ORDER BY name COLLATE utf8mb4_general_ci, id;
SELECT 'T12', BINARY 'a' = 'A';
