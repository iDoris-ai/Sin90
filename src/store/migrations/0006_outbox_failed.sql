-- 0006 (T3.3.1, design DESIGN-LIFEOS.md §2 #15, §4.1; spec.md M3 "outbox";
-- 错误处理): outbox status widens from `pending|done` to
-- `pending|done|failed`. `sin90_outbox.status` has never carried a CHECK
-- constraint (see 0001's comment above `CREATE TABLE sin90_outbox`), so the
-- new value needs no table rebuild — the status set is enforced in Rust
-- (`store::repo::upsert_outbox`), not by SQLite. Four columns support
-- retry/failure bookkeeping:
--
--   failure_kind     permanent-failure classification — one of
--                     `forbidden` | `quota_exceeded` | `invalid_params`
--                     (spec.md "错误处理"); NULL until the row is `failed`.
--   last_error       last error message/detail seen for this row; NULL
--                     until any attempt has failed.
--   attempts         retry counter; 0 until the first attempt, reset to 0
--                     whenever `upsert_outbox` overwrites the row with a
--                     fresh desired state.
--   next_attempt_at  earliest time a retryable (rate_limited/busy/timeout/
--                     disconnected/not_ready/draining) attempt may run
--                     next; NULL when no retry is scheduled. Written by the
--                     reconciler (T3.3.2), not by this task.
--
-- Existing rows: `ALTER TABLE ... ADD COLUMN` with a fixed/NULL default
-- leaves every existing row's `id/kind/dedup_key/desired/status/created_at/
-- done_at` untouched; `attempts` becomes 0 and the other three become NULL
-- for pre-existing rows, matching "not yet retried, not failed".

ALTER TABLE sin90_outbox ADD COLUMN failure_kind TEXT NULL;
ALTER TABLE sin90_outbox ADD COLUMN last_error TEXT NULL;
ALTER TABLE sin90_outbox ADD COLUMN attempts INTEGER NOT NULL DEFAULT 0;
ALTER TABLE sin90_outbox ADD COLUMN next_attempt_at TEXT NULL;
