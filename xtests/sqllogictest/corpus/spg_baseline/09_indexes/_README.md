# 09 — Indexes

CREATE INDEX btree (basic, multi-column, expression, partial), GIN
on jsonb / tsvector / trgm. GIN-on-jsonb ships in v7.37.8;
trgm ships through v7.22's pg_trgm work; FTS via tsvector lands in
v7.22 + v7.23 jumbo-page work.
