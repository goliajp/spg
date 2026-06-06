-- v7.15.0 — pg_dump-shape COPY block. Includes:
--   * TIMESTAMPTZ with +00, +09, -05, +05:30 offsets
--   * TIMESTAMPTZ NULL via \N
--   * BYTEA hex literal
--   * TEXT[] with single + multi element
--   * TEXT with tab, backslash, quote (COPY-escape decoded)
--   * BOOLEAN as t/f
-- 8 rows posts + 5 rows events = 13 total. After load the gate
-- asserts SELECT count(*) = 8 / 5 exactly.

COPY public.posts (id, title, body, is_draft, tags, metadata, bin, score_micros, created_at, updated_at) FROM stdin;
1	Hello, world	first post	f	{intro,hello}	\N	\\x68656c6c6f	100	2023-10-27 12:00:00+00	2023-10-27 12:00:00+00
2	East-of-UTC post	body two	f	{follow-up}	{"k":"v"}	\\x	0	2024-06-15 09:30:00+09	\N
3	West-of-UTC post	body three	t	{}	\N	\N	-1	2025-12-31 23:59:59-05	2026-01-01 04:59:59+00
4	Tab\there	embedded\ttab	f	{tabs}	\N	\N	0	2026-01-01 00:00:00 UTC	\N
5	Quotes 'matter'	body with 'quote'	t	{edge-cases}	\N	\N	0	2026-01-01 00:00:00Z	\N
6	Backslash \\\\	literal \\\\ backslash	f	{edge-cases}	\N	\N	0	2026-01-01 00:00:00	\N
7	Half-hour zone	IST input	f	{ist}	\N	\N	0	2026-03-01 09:00:00+05:30	\N
8	Sub-second	fraction	f	{ms}	\N	\N	0	2026-04-15 14:30:45.678901+00	\N
\.


COPY public.events (id, name, at, payload) FROM stdin;
1	signin	2026-01-15 10:00:00+00	{"ip":"127.0.0.1"}
2	signin	2026-01-15 10:00:01+00	\N
3	post-create	2026-01-15 10:05:00+09	{"post_id":42}
4	post-update	2026-01-15 10:10:00-05	\N
5	signout	2026-01-15 11:00:00+00	\N
\.
