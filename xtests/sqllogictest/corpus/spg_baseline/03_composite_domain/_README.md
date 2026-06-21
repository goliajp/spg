# 03 — Composite types + DOMAIN

CREATE TYPE … AS (a INT, b TEXT) composites and CREATE DOMAIN … AS …
CHECK domains. These shipped in v7.37.5-ζ-B (composite + domain).
Coverage here is shape-level — the dropin panel and dump-compat
gates exercise the full surface.
