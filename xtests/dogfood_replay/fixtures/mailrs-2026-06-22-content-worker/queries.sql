-- mailrs content_worker hot query.
-- Prod baseline (2026-06-22): warm ~3-5s, p95 ~5.3s, max ~19s.
-- Post-fix expected: warm < 10ms, p95 < 15ms, cold < 100ms.

SELECT m.id, m.sender, m.maildir_id, mb.user_address
  FROM messages m
  JOIN mailboxes mb ON m.mailbox_id = mb.id
 WHERE m.size > 0
   AND NOT EXISTS (
       SELECT 1 FROM attachment_content ac WHERE ac.message_id = m.id
   )
 ORDER BY m.id DESC
 LIMIT 64;
