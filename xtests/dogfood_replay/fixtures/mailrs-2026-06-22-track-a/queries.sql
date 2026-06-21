-- Track A — full /api/conversations SQL.
-- Real prod SQL captured in
-- stables/mailrs/.claude/notes/spg-7.37.3-prod-conversations-real-sql-2026-06-18.md
-- Stitches conversations × messages × labels with a windowed
-- ranking + a NOT EXISTS attachment filter. The slowest single
-- statement in the mailrs hot path.

WITH ranked AS (
    SELECT
        c.id              AS conversation_id,
        c.subject,
        c.last_message_at,
        m.id              AS message_id,
        m.sender,
        m.received_at,
        ROW_NUMBER() OVER (
            PARTITION BY c.id
            ORDER BY m.received_at DESC
        ) AS rn
    FROM conversations c
    JOIN messages m ON m.conversation_id = c.id
    WHERE c.user_id = 1
      AND c.deleted_at IS NULL
)
SELECT r.conversation_id,
       r.subject,
       r.last_message_at,
       r.message_id,
       r.sender
  FROM ranked r
 WHERE r.rn = 1
   AND NOT EXISTS (
       SELECT 1
         FROM message_labels ml
         JOIN labels l ON l.id = ml.label_id
        WHERE ml.message_id = r.message_id
          AND l.name = 'spam'
   )
 ORDER BY r.last_message_at DESC
 LIMIT 50;
