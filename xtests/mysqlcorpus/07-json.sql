-- MySQL's JSON type and its own operators.
SELECT 'T01', JSON_EXTRACT('{"a":1,"b":[2,3]}','$.a');
SELECT 'T02', '{"a":1}' -> '$.a', '{"a":"x"}' ->> '$.a';
SELECT 'T03', JSON_UNQUOTE(JSON_EXTRACT('{"a":"x"}','$.a'));
SELECT 'T04', JSON_TYPE('{"a":1}'), JSON_TYPE('[1]'), JSON_TYPE('1'), JSON_TYPE('"s"');
SELECT 'T05', JSON_VALID('{"a":1}'), JSON_VALID('{a:1}');
SELECT 'T06', JSON_LENGTH('[1,2,3]'), JSON_DEPTH('{"a":{"b":1}}');
SELECT 'T07', JSON_KEYS('{"a":1,"b":2}');
SELECT 'T08', JSON_ARRAY(1,'a',NULL), JSON_OBJECT('k',1,'j','x');
SELECT 'T09', JSON_CONTAINS('[1,2,3]','2'), JSON_CONTAINS_PATH('{"a":1}','one','$.a');
SELECT 'T10', JSON_SET('{"a":1}','$.b',2), JSON_INSERT('{"a":1}','$.b',2), JSON_REPLACE('{"a":1}','$.a',9);
SELECT 'T11', JSON_REMOVE('{"a":1,"b":2}','$.a'), JSON_MERGE_PATCH('{"a":1}','{"b":2}');
SELECT 'T12', JSON_QUOTE('a"b'), JSON_ARRAY_APPEND('[1]','$',2);
SELECT 'T13', CAST('{"a":1}' AS JSON) = CAST('{"a":1}' AS JSON);
SELECT 'T14', JSON_PRETTY('{"a":1}');
