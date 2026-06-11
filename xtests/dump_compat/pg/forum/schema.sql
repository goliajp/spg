--
-- PostgreSQL database dump
--

\restrict CWPEzkcvMovPMWSTvCH5YcpTQsk7DjChTbAAPgijEMtWhhJLSnWnos43Dfo2h8A

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
-- Name: forums; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.forums (
    id bigint NOT NULL,
    parent_id bigint,
    name text NOT NULL,
    slug text NOT NULL,
    sort_order integer DEFAULT 0 NOT NULL
);


--
-- Name: forums_id_seq; Type: SEQUENCE; Schema: public; Owner: -
--

CREATE SEQUENCE public.forums_id_seq
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;


--
-- Name: forums_id_seq; Type: SEQUENCE OWNED BY; Schema: public; Owner: -
--

ALTER SEQUENCE public.forums_id_seq OWNED BY public.forums.id;


--
-- Name: topics; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.topics (
    id bigint NOT NULL,
    forum_id bigint NOT NULL,
    title text NOT NULL,
    pinned boolean DEFAULT false NOT NULL,
    locked boolean DEFAULT false NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    last_reply_at timestamp with time zone
);


--
-- Name: topics_id_seq; Type: SEQUENCE; Schema: public; Owner: -
--

CREATE SEQUENCE public.topics_id_seq
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;


--
-- Name: topics_id_seq; Type: SEQUENCE OWNED BY; Schema: public; Owner: -
--

ALTER SEQUENCE public.topics_id_seq OWNED BY public.topics.id;


--
-- Name: forums id; Type: DEFAULT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.forums ALTER COLUMN id SET DEFAULT nextval('public.forums_id_seq'::regclass);


--
-- Name: topics id; Type: DEFAULT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.topics ALTER COLUMN id SET DEFAULT nextval('public.topics_id_seq'::regclass);


--
-- Name: forums forums_parent_id_slug_key; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.forums
    ADD CONSTRAINT forums_parent_id_slug_key UNIQUE (parent_id, slug);


--
-- Name: forums forums_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.forums
    ADD CONSTRAINT forums_pkey PRIMARY KEY (id);


--
-- Name: topics topics_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.topics
    ADD CONSTRAINT topics_pkey PRIMARY KEY (id);


--
-- Name: idx_forums_parent; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_forums_parent ON public.forums USING btree (parent_id);


--
-- Name: idx_topics_active; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_topics_active ON public.topics USING btree (last_reply_at DESC) WHERE (locked = false);


--
-- Name: idx_topics_forum; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_topics_forum ON public.topics USING btree (forum_id, pinned DESC, last_reply_at DESC NULLS LAST);


--
-- Name: forums forums_parent_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.forums
    ADD CONSTRAINT forums_parent_id_fkey FOREIGN KEY (parent_id) REFERENCES public.forums(id) ON DELETE CASCADE;


--
-- Name: topics topics_forum_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.topics
    ADD CONSTRAINT topics_forum_id_fkey FOREIGN KEY (forum_id) REFERENCES public.forums(id) ON DELETE CASCADE;


--
-- PostgreSQL database dump complete
--

\unrestrict CWPEzkcvMovPMWSTvCH5YcpTQsk7DjChTbAAPgijEMtWhhJLSnWnos43Dfo2h8A

