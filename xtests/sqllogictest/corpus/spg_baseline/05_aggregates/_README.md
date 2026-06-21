# 05 — Aggregates + window functions

Aggregates (COUNT/SUM/AVG/MIN/MAX), GROUP BY, HAVING, DISTINCT, and
the window-function surface (ROW_NUMBER, RANK, LAG, LEAD, FILTER).
GROUP BY ALL shipped in v6.4.1; DISTINCT in aggregate args via
v7.25.2 round-19; string_agg / array_agg via the agg-typed-cols
work in v7.26.
