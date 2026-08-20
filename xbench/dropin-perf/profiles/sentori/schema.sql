-- Sentori's two hot shapes, reduced to what makes them expensive.
--
-- Ingest is a single-row insert into a table carrying a GIN index on a
-- jsonb column and a BRIN index on a timestamp; the dashboard is
-- aggregates over a time window. Everything here is valid on both
-- PostgreSQL 18 and SPG, so the same file seeds both legs.
DROP TABLE IF EXISTS events;
CREATE TABLE events (
  id          bigserial PRIMARY KEY,
  project_id  int         NOT NULL,
  kind        text        NOT NULL,
  traits      jsonb       NOT NULL,
  received_at timestamp   NOT NULL
);
-- The two indexes sentori would least expect a from-scratch engine to
-- match: jsonb_path_ops is the containment index the audience filter
-- rides on, and BRIN is what makes a 90-day table answer a one-day
-- question without a sequential scan.
CREATE INDEX events_traits ON events USING gin (traits jsonb_path_ops);
CREATE INDEX events_time   ON events USING brin (received_at);
CREATE INDEX events_kind   ON events (project_id, kind);

INSERT INTO events (project_id, kind, traits, received_at)
SELECT (g % 8) + 1,
       (ARRAY['open','click','deliver','bounce'])[(g % 4) + 1],
       jsonb_build_object(
         'plan',    (ARRAY['free','pro','team'])[(g % 3) + 1],
         'country', (ARRAY['jp','us','de','br'])[(g % 4) + 1],
         'version', ((g % 40) + 1)::text,
         'seat',    g % 500
       ),
       timestamp '2026-05-01 00:00:00' + ((g % 129600) || ' minutes')::interval
FROM generate_series(1, 200000) g;

ANALYZE events;
