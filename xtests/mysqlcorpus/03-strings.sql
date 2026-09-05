-- String functions in MySQL's own spelling, including the ones with no
-- PostgreSQL twin.
SELECT 'T01', LOCATE('b','abc'), LOCATE('b','abc',3), INSTR('abc','b'), POSITION('b' IN 'abc');
SELECT 'T02', SUBSTRING_INDEX('a,b,c',',',2), SUBSTRING_INDEX('a,b,c',',',-1);
SELECT 'T03', CHAR_LENGTH('日本'), LENGTH('日本'), OCTET_LENGTH('日本');
SELECT 'T04', LPAD('7',3,'0'), RPAD('7',3,'0'), SPACE(3) = '   ';
SELECT 'T05', REPLACE('aXbXc','X','-'), REVERSE('abc'), REPEAT('ab',3);
SELECT 'T06', TRIM('  a  '), TRIM(LEADING 'x' FROM 'xxaxx'), TRIM(BOTH 'x' FROM 'xxaxx');
SELECT 'T07', LEFT('abcdef',2), RIGHT('abcdef',2), MID('abcdef',2,3);
SELECT 'T08', UPPER('aä'), LOWER('AÄ'), UCASE('a'), LCASE('A');
SELECT 'T09', ASCII('A'), CHAR(65), ORD('A'), ORD('日');
SELECT 'T10', 'a' LIKE 'A', 'a' LIKE BINARY 'A', 'abc' REGEXP '^a';
SELECT 'T11', STRCMP('a','b'), STRCMP('b','a'), STRCMP('a','a');
SELECT 'T12', QUOTE("a'b"), CONCAT_WS(',', 'a', 'b', 'c');
SELECT 'T13', EXPORT_SET(5,'Y','N','',4), MAKE_SET(5,'a','b','c');
SELECT 'T14', FIND_IN_SET('b','a,b,c'), FIND_IN_SET('z','a,b,c');
SELECT 'T15', HEX(CAST('a' AS BINARY)), TO_BASE64('abc'), FROM_BASE64('YWJj');
SELECT 'T16', name, LENGTH(name) FROM mc_t ORDER BY name, id;
