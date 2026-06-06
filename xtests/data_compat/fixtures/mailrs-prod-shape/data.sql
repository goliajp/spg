-- v7.16.1 — pg_dump --disable-triggers shape over messages +
-- attachments. The DISABLE TRIGGER wrapper around messages
-- means the server-side `mark_search_vector_trg` MUST NOT fire
-- on these rows — they carry their already-computed
-- search_vector from prod. ENABLE epilogue restores firing.

-- Server-side preamble pg_dump emits.
SET statement_timeout = 0;
SET client_encoding = 'UTF8';
SELECT pg_catalog.set_config('search_path', '', false);

-- messages — wrapped in DISABLE / ENABLE TRIGGER ALL.
ALTER TABLE public.messages DISABLE TRIGGER ALL;

COPY public.messages (id, subject, body, search_vector, created_at) FROM stdin;
1	Welcome to mailrs	first body	'welcom':1 'mailr':3	2026-06-01 00:00:00+00
2	Inbox digest	body two	'inbox':1 'digest':2	2026-06-02 00:00:00+00
3	Tab\there	embedded\ttab	'tab':1 'tabb':2	2026-06-03 00:00:00+00
4	Quotes 'matter'	body with 'quote'	'quot':1,2 'matter':2	2026-06-04 00:00:00+00
5	Half-tagged	body five		2026-06-05 00:00:00+00
\.

ALTER TABLE public.messages ENABLE TRIGGER ALL;

-- attachments — no triggers to disable, plain COPY.
COPY public.attachments (id, message_id, name, bytes) FROM stdin;
1	1	hello.txt	\\x68656c6c6f
2	1	world.bin	\\x776f726c64
3	2	digest.pdf	\\x25504446
4	5	tagged.png	\N
\.
