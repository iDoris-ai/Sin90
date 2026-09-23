-- 0009 (T5.1.1, design DESIGN-LIFEOS.md §11.6): engine-ladder call-record
-- columns + the generic key/value settings table AI settings (and future
-- settings) live in.
--
-- `sin90_ai_calls` grows from "one row per AI decision, coarse" (0001) to
-- "one row per ladder STEP, atomic with the proposal it produced" (§11.3.5):
--   - `run_id`       groups every row from one `POST /ai/*` trigger.
--   - `proposal_id`  filled ONLY on the `ok=1` row a `submit` produced (same
--                     transaction, §11.5's `AiSink::submit`) — no FK: a
--                     failed/non-producing row never gets one.
--   - `served_tier`  the model's ACTUAL `result.tier` (local|remote); NULL
--                     for reflex rows and rows that never got a reply.
--   - `model_id`, `prompt_tokens`, `completion_tokens` mirror `result`/
--     `result.usage` (§11.3.4 L7).
--   - `error_kind`   the failure-table string (§11.3.4) or the reflex
--                     `undecided`/`no_match`; NULL when `ok = 1`.
--
-- Takes migration slot 0009, the next free one after 0008
-- (`outbox_migrations_are_contiguous_no_gaps`, src/store/repo.rs).

ALTER TABLE sin90_ai_calls ADD COLUMN run_id            TEXT;
ALTER TABLE sin90_ai_calls ADD COLUMN proposal_id       TEXT;
ALTER TABLE sin90_ai_calls ADD COLUMN served_tier       TEXT;
ALTER TABLE sin90_ai_calls ADD COLUMN model_id          TEXT;
ALTER TABLE sin90_ai_calls ADD COLUMN prompt_tokens     INTEGER;
ALTER TABLE sin90_ai_calls ADD COLUMN completion_tokens INTEGER;
ALTER TABLE sin90_ai_calls ADD COLUMN error_kind        TEXT;
CREATE INDEX idx_sin90_ai_calls_run      ON sin90_ai_calls(run_id);
CREATE INDEX idx_sin90_ai_calls_proposal ON sin90_ai_calls(proposal_id);

-- Generic scalar key/value store (§11.3.2): today only `ai.executive_enabled`
-- (`GET|PUT /settings/ai`), but the shape is generic on purpose — a future
-- setting does not need another migration, just another key.
CREATE TABLE sin90_settings (
    key        TEXT PRIMARY KEY,
    value      TEXT NOT NULL,          -- JSON scalar
    updated_at TEXT NOT NULL
);
