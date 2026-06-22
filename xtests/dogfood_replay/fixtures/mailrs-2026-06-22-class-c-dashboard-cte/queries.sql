-- Class C — unread thread count dashboard CTE.
-- Source: stables/mailrs/.claude/notes/spg-7.37.6-prod-pool-cascade-4th-recurrence-2026-06-22.md
-- mailrs source: crates/mailbox/src/pg/message_ops/read.rs:212-233
-- Prod baseline (2026-06-22 2h window): 3 hits at avg 3.6s. PG18 on same data: <100 ms cold.
-- Note from mailrs source comment: this is ALREADY a rewrite of the cleaner standard-SQL form
-- (FROM-clause derived table + FILTER aggregate) that spg 7.30.3 couldn't parse — mailrs is paying
-- a readability tax to stay compatible.
--
-- Shape: CTE wraps `messages × mailboxes JOIN + 2 NOT EXISTS subqueries`, then HAVING with
-- BOOL_OR / COUNT(CASE) / LOWER(COALESCE(... scalar-subq ...)) NOT LIKE pattern. The outer SELECT
-- is just COUNT(*) over the CTE.

WITH unread_threads AS (
  SELECT m.thread_id
  FROM messages m
  JOIN mailboxes mb ON m.mailbox_id = mb.id
  WHERE mb.user_address = 'lihao@golia.ai'
    AND m.thread_id != ''
    AND NOT EXISTS (
      SELECT 1 FROM snoozed_conversations sc
      WHERE sc.thread_id = m.thread_id
        AND sc.account_address = mb.user_address
        AND sc.snoozed_until > NOW()
    )
    AND NOT EXISTS (
      SELECT 1 FROM email_analysis ea
      WHERE ea.message_id = m.id
        AND ea.category IN ('spam', 'scam')
    )
  GROUP BY m.thread_id
  HAVING BOOL_OR(m.archived) = false
     AND COUNT(CASE WHEN (m.flags & 1) = 0 THEN 1 END) > 0
     AND LOWER(COALESCE(
           (SELECT m_last.sender FROM messages m_last
            WHERE m_last.thread_id = m.thread_id
            ORDER BY m_last.internal_date DESC LIMIT 1),
           '')
         ) NOT LIKE '%' || LOWER('lihao@golia.ai') || '%'
)
SELECT COUNT(*) FROM unread_threads;
