-- orig.spg_jsonb_at_op — SPG-originated JSONB `@>` containment test.
--
-- Surface added in v7.37.6-A (JSONB op acceptance). PG18 also has
-- `@>` so this same SQL runs on the PG oracle for parity; MariaDB
-- 11 has the same operator via JSON_CONTAINS-equivalent; MySQL 8
-- requires JSON_CONTAINS() rewrite — adjust_*() handles the call
-- shape difference (P1 fill).

CREATE TABLE jsonb_docs (
    id INT NOT NULL PRIMARY KEY,
    doc JSONB NOT NULL
);

INSERT INTO jsonb_docs (id, doc) VALUES
    (1, '{"k": "alpha", "tags": ["a", "b"]}'),
    (2, '{"k": "beta",  "tags": ["b", "c"]}'),
    (3, '{"k": "gamma", "tags": ["a", "c"]}');

SELECT id
FROM jsonb_docs
WHERE doc @> '{"tags": ["a"]}';
