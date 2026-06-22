-- Class B — unread tally (mailbox-scoped NOT EXISTS).
-- Source: stables/mailrs/.claude/notes/spg-7.37.6-prod-pool-cascade-4th-recurrence-2026-06-22.md
-- mailrs source: crates/mailbox/src/pg/mailbox_ops.rs:167-174
-- Prod baseline (2026-06-22 2h window): 8 hits, contributing to 25 budget breaches at avg 3.6s.
-- PG18 on same data: <100 ms cold.
-- Expected per [[feedback-perf-is-the-reverse-scale]] — SPG must reach PG18 parity (≤ PG18 wire) or better.
--
-- Shape: single-table COUNT(*) + bitwise flag predicate + correlated NOT EXISTS subquery against email_analysis,
-- where the inner predicate is `category IN ('spam', 'scam')`. Anti-join shape — PG18 picks hash-anti or
-- merge-anti, while SPG's current planner appears to drive the subquery per row on prod shape.
--
-- mailbox_id resolved inline from `(user_address='lihao@golia.ai', name='INBOX')` so the fixture is
-- self-contained and uses the real prod path. Both PG18 and SPG should fold the inner SELECT to a
-- constant before executing the outer COUNT.

SELECT COUNT(*) FROM messages m
WHERE m.mailbox_id = (
        SELECT id FROM mailboxes
        WHERE user_address = 'lihao@golia.ai' AND name = 'INBOX'
        LIMIT 1
      )
  AND (m.flags & 1) = 0
  AND NOT EXISTS (
        SELECT 1 FROM email_analysis ea
        WHERE ea.message_id = m.id
          AND ea.category IN ('spam', 'scam')
      );
