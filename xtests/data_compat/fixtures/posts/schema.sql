-- v7.15.0 — data round-trip gate fixture
-- Schema covers the column shapes pg_dump emits and the bug
-- classes mailrs round-8 surfaced: TIMESTAMPTZ with explicit
-- offset, BYTEA, JSONB, BOOLEAN, TEXT[], INT[], plain TEXT
-- with embedded tabs / quotes / backslashes.

CREATE TABLE posts (
    id BIGINT NOT NULL,
    title TEXT NOT NULL,
    body TEXT NOT NULL DEFAULT '',
    is_draft BOOLEAN NOT NULL DEFAULT TRUE,
    tags TEXT[] NOT NULL DEFAULT ARRAY[]::TEXT[],
    metadata TEXT,
    bin BYTEA,
    score_micros BIGINT NOT NULL DEFAULT 0,
    created_at TIMESTAMPTZ NOT NULL,
    updated_at TIMESTAMPTZ
);

CREATE TABLE events (
    id BIGINT NOT NULL,
    name TEXT NOT NULL,
    at TIMESTAMPTZ NOT NULL,
    payload TEXT
);
