# 13 — Recovery + transactions

BEGIN / COMMIT / ROLLBACK semantics in the embedded engine.
Crash-recovery smoke + WAL-replay end-to-end checks live in the
data_compat gate (`xtests/data_compat/`) — they need a process kill
+ restart cycle that sqllogictest's in-process runner can't model.
