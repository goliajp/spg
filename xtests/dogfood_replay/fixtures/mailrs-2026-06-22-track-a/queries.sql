-- Track A — /api/conversations REAL prod SQL.
-- Source: stables/mailrs/.claude/notes/spg-7.37.3-prod-conversations-real-sql-2026-06-18.md
-- mailrs source: crates/mailbox/src/pg/thread_ops/query.rs:135–165 (list_conversations)
-- Prod baseline (2026-06-22): median 3.7s, p95 4.4s, max 8.3s
-- Post-fix expected: warm < 5ms, p95 < 10ms, cold < 100ms

SELECT m.thread_id, MAX(m.subject), string_agg(DISTINCT m.sender, ','),
       COUNT(DISTINCT CASE WHEN m.message_id != ''
                           THEN m.message_id
                           ELSE CAST(m.id AS TEXT) END),
       COUNT(DISTINCT CASE WHEN (m.flags & 1) = 0
                           THEN CASE WHEN m.message_id != ''
                                     THEN m.message_id
                                     ELSE CAST(m.id AS TEXT) END
                           END),
       MAX(m.internal_date),
       COALESCE((SELECT ea.category FROM email_analysis ea
                   JOIN messages m2 ON ea.message_id = m2.id
                  WHERE m2.thread_id = m.thread_id
                  ORDER BY m2.internal_date DESC LIMIT 1),
                'general'),
       BOOL_OR((m.flags & 4) != 0),
       COALESCE(
         (SELECT LEFT(ea_snip.summary, 80) FROM email_analysis ea_snip
            JOIN messages m_snip ON ea_snip.message_id = m_snip.id
           WHERE m_snip.thread_id = m.thread_id
             AND ea_snip.summary IS NOT NULL
             AND ea_snip.summary != ''
           ORDER BY m_snip.internal_date DESC LIMIT 1),
         (SELECT LEFT(m3.text_body, 80) FROM messages m3
           WHERE m3.thread_id = m.thread_id
             AND m3.text_body IS NOT NULL
             AND m3.text_body != ''
           ORDER BY m3.internal_date DESC LIMIT 1),
         ''),
       BOOL_OR(m.pinned),
       BOOL_OR(m.archived),
       COALESCE((array_agg(m.importance_level
                           ORDER BY m.importance_score DESC NULLS LAST))[1],
                'normal'),
       COALESCE(MAX(m.importance_score), 0.0),
       COALESCE(BOOL_OR(ea.requires_action), false),
       COALESCE((array_agg(m.sender ORDER BY m.internal_date DESC))[1], ''),
       COUNT(DISTINCT CASE WHEN mb.name = 'Sent' AND m.message_id != ''
                           THEN m.message_id
                           WHEN mb.name = 'Sent'
                           THEN CAST(m.id AS TEXT) END)
  FROM messages m
       JOIN mailboxes mb ON m.mailbox_id = mb.id
       LEFT JOIN email_analysis ea ON ea.message_id = m.id
 WHERE mb.user_address = 'lihao@golia.jp'
   AND thread_id != ''
   AND NOT EXISTS (SELECT 1 FROM snoozed_conversations sc
                    WHERE sc.thread_id = m.thread_id
                      AND sc.account_address = mb.user_address
                      AND sc.snoozed_until > NOW())
 GROUP BY m.thread_id
 HAVING BOOL_OR(m.archived) = false
    AND BOOL_OR(mb.name != 'Sent') = true
 ORDER BY BOOL_OR(m.pinned) DESC, MAX(m.internal_date) DESC
 LIMIT 50;
