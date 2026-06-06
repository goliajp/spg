-- v7.16.1 — mailrs round-9 data-shape fixture.
-- Locks the 3 surfaces round-9 flagged + the NEW.col trigger
-- regression I caught in this same release. Keeps mailrs-prod
-- patterns in the gate so the same class can't silently break
-- again.

-- A.2.a — TSVECTOR-typed column. Mirrors mailrs's
-- `messages.search_vector` shape (TSVECTOR + GIN index).
CREATE TABLE messages (
    id BIGINT NOT NULL,
    subject TEXT NOT NULL,
    body TEXT NOT NULL,
    search_vector TSVECTOR,
    created_at TIMESTAMPTZ NOT NULL
);
CREATE INDEX messages_sv_gin ON messages USING gin(search_vector);

-- Server-side trigger that auto-populates search_vector from
-- subject + body. mailrs has the same shape on migrate-016.
-- This MUST fire when triggers are enabled and MUST be skipped
-- inside a DISABLE TRIGGER ALL wrapper.
CREATE FUNCTION mark_search_vector() RETURNS TRIGGER LANGUAGE plpgsql AS $$
BEGIN
    NEW.search_vector := to_tsvector('english', NEW.subject || ' ' || NEW.body);
    RETURN NEW;
END;
$$;
CREATE TRIGGER mark_search_vector_trg BEFORE INSERT ON messages
    FOR EACH ROW EXECUTE FUNCTION mark_search_vector();

-- Companion table — exercises the FK + dump-shape cascade
-- when the parent's INSERT block is wrapped in DISABLE TRIGGER.
CREATE TABLE attachments (
    id BIGINT NOT NULL,
    message_id BIGINT NOT NULL,
    name TEXT NOT NULL,
    bytes BYTEA
);
