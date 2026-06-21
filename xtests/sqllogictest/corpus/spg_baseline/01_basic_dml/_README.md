# 01 — Basic DML

The four-column DML surface SPG must support: SELECT shape (projection,
WHERE, ORDER, LIMIT), INSERT VALUES + INSERT SELECT, UPDATE SET WHERE,
DELETE FROM WHERE, plus the PG flagship clauses ON CONFLICT and RETURNING.

Shipped through v7.17.x baseline; some clauses (RETURNING in DELETE,
ON CONFLICT DO NOTHING) refined through v7.21–v7.25.
