WITH matched AS (
   SELECT id FROM messages WHERE search_vector @@ plainto_tsquery('simple', $2)
   UNION SELECT id FROM messages WHERE subject IS NOT NULL AND subject != '' AND subject ILIKE $3
   UNION SELECT id FROM messages WHERE sender IS NOT NULL AND sender != '' AND sender ILIKE $3
   UNION SELECT id FROM messages WHERE recipients IS NOT NULL AND recipients != '' AND recipients ILIKE $3
   UNION SELECT id FROM messages WHERE text_body IS NOT NULL AND text_body != '' AND text_body ILIKE $3
   UNION SELECT id FROM messages WHERE clean_text IS NOT NULL AND clean_text != '' AND clean_text ILIKE $3
   UNION SELECT message_id FROM attachment_content WHERE extracted_text ILIKE $3
 ),
 cands AS (
   SELECT m_all.id
     FROM messages m_all
    WHERE m_all.thread_id IN (SELECT thread_id FROM messages WHERE id IN (SELECT id FROM matched))
 )
 SELECT m.thread_id, MAX(m.subject), string_agg(DISTINCT m.sender, ','),
        COUNT(DISTINCT CASE WHEN m.message_id != '' THEN m.message_id ELSE CAST(m.id AS TEXT) END),
        COUNT(DISTINCT CASE WHEN (m.flags & 1) = 0 THEN CASE WHEN m.message_id != '' THEN m.message_id ELSE CAST(m.id AS TEXT) END END),
        MAX(m.internal_date),
        COALESCE((SELECT ea2.category FROM email_analysis ea2
                  JOIN messages m2 ON ea2.message_id = m2.id
                  WHERE m2.thread_id = m.thread_id
                  ORDER BY m2.internal_date DESC LIMIT 1), 'general'),
        BOOL_OR((m.flags & 4) != 0),
        COALESCE(
          (SELECT ea_snip.summary FROM email_analysis ea_snip
           JOIN messages m_snip ON ea_snip.message_id = m_snip.id
           WHERE m_snip.thread_id = m.thread_id AND ea_snip.summary IS NOT NULL AND ea_snip.summary != ''
           ORDER BY m_snip.internal_date DESC LIMIT 1),
          (SELECT LEFT(m3.text_body, 120) FROM messages m3
           WHERE m3.thread_id = m.thread_id AND m3.text_body IS NOT NULL AND m3.text_body != ''
           ORDER BY m3.internal_date DESC LIMIT 1),
          ''),
        BOOL_OR(m.pinned),
        BOOL_OR(m.archived),
        COALESCE((array_agg(m.importance_level ORDER BY m.importance_score DESC NULLS LAST))[1], 'normal'),
        COALESCE(MAX(m.importance_score), 0.0),
        COALESCE(BOOL_OR(ea.requires_action), false),
        COALESCE((array_agg(m.sender ORDER BY m.internal_date DESC))[1], ''),
        COUNT(DISTINCT CASE WHEN mb.name  = 'Sent' AND m.message_id != '' THEN m.message_id WHEN mb.name  = 'Sent' THEN CAST(m.id AS TEXT) END)
 FROM messages m
      JOIN cands ON cands.id = m.id
      JOIN mailboxes mb ON m.mailbox_id = mb.id
      LEFT JOIN email_analysis ea ON ea.message_id = m.id
 WHERE mb.user_address = $1 AND thread_id != ''
 GROUP BY m.thread_id HAVING BOOL_OR(m.archived) = false
 ORDER BY MAX(CASE WHEN m.search_vector @@ plainto_tsquery('simple', $2) THEN ts_rank(m.search_vector, plainto_tsquery('simple', $2)) ELSE 0 END) DESC, MAX(m.internal_date) DESC LIMIT $4
