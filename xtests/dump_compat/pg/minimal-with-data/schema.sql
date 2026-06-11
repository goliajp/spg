--
-- PostgreSQL database dump
--

\restrict zF1kejxI0EFvaKVPBMGuMycJ5nd196cRoUse7LIueHtCVbmaDRHLN8jeR9vyMcH

-- Dumped from database version 18.4 (Debian 18.4-1.pgdg13+1)
-- Dumped by pg_dump version 18.4 (Debian 18.4-1.pgdg13+1)

SET statement_timeout = 0;
SET lock_timeout = 0;
SET idle_in_transaction_session_timeout = 0;
SET transaction_timeout = 0;
SET client_encoding = 'UTF8';
SET standard_conforming_strings = on;
SELECT pg_catalog.set_config('search_path', '', false);
SET check_function_bodies = false;
SET xmloption = content;
SET client_min_messages = warning;
SET row_security = off;

SET default_tablespace = '';

SET default_table_access_method = heap;

--
-- Name: posts; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.posts (
    id bigint NOT NULL,
    title text NOT NULL,
    body text DEFAULT ''::text NOT NULL,
    tags text[] DEFAULT ARRAY[]::text[] NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL
);


--
-- Name: TABLE posts; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON TABLE public.posts IS 'Blog posts';


--
-- Name: COLUMN posts.title; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.posts.title IS 'Post title';


--
-- Name: posts_id_seq; Type: SEQUENCE; Schema: public; Owner: -
--

CREATE SEQUENCE public.posts_id_seq
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;


--
-- Name: posts_id_seq; Type: SEQUENCE OWNED BY; Schema: public; Owner: -
--

ALTER SEQUENCE public.posts_id_seq OWNED BY public.posts.id;


--
-- Name: posts id; Type: DEFAULT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.posts ALTER COLUMN id SET DEFAULT nextval('public.posts_id_seq'::regclass);


--
-- Data for Name: posts; Type: TABLE DATA; Schema: public; Owner: -
--

COPY public.posts (id, title, body, tags, created_at) FROM stdin;
1	Hello, world	first post	{intro,hello}	2026-06-11 03:58:03.020128+00
2	Second post	body two	{follow-up}	2026-06-11 03:58:03.020128+00
3	Tab\\thandling	COPY round-trip must escape \\t \\n \\\\ safely	{}	2026-06-11 03:58:03.020128+00
4	Quotes 'matter'	and so do backslashes \\\\	{edge-cases}	2026-06-11 03:58:03.020128+00
\.


--
-- Name: posts_id_seq; Type: SEQUENCE SET; Schema: public; Owner: -
--

SELECT pg_catalog.setval('public.posts_id_seq', 4, true);


--
-- Name: posts posts_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.posts
    ADD CONSTRAINT posts_pkey PRIMARY KEY (id);


--
-- Name: idx_posts_created; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_posts_created ON public.posts USING btree (created_at DESC);


--
-- PostgreSQL database dump complete
--

\unrestrict zF1kejxI0EFvaKVPBMGuMycJ5nd196cRoUse7LIueHtCVbmaDRHLN8jeR9vyMcH

