-- 0015 (T5.7.2 review round 2 — H2/M5): two independent, small additions,
-- both infrastructure for the "被拒建议条件压制 + 待定重新分类" fixes —
-- landed together in one migration file (both trivial, no shared rationale
-- beyond "small store-layer addition this round") but each read/written by
-- SEPARATE Rust code paths, per pre-pr-check SZ-4 the migration file itself
-- stays its own commit either way.
--
-- H2: `sin90_classify_evals` — "when did classify last actually LOOK AT and
-- evaluate this task" (one row per task, upserted by `AiSink::
-- record_classify_eval`, `store/ai_port.rs`), independent of whether that
-- look produced a proposal. Without this, `AiReadModel::inbox`/`inbox_task`'s
-- 待定 retry gate (`updated_at < newest eligible Direction`) had no exit
-- condition: a triage-parked task the model re-examines and STILL cannot
-- place (`none`/low-confidence/no-conclusion — none of which touch
-- `sin90_tasks.updated_at`) keeps re-qualifying as a target on every single
-- run for as long as that same newest Direction stays the newest one,
-- permanently occupying one of `classify::MAX_CLASSIFY_TASK_IDS` (20) slots
-- and starving genuinely new inbox tasks once ~20 such never-resolving
-- retries accumulate. The gate's comparand becomes `MAX(eligible Direction
-- created_at) > COALESCE(evaluated_at, <entered-待定-at>)`: a fresh
-- evaluation moves the floor forward even when it changes nothing else, so
-- the task drops out of contention again until a Direction newer still
-- shows up.
--
-- `task_id` is the PRIMARY KEY (one row per task, always upserted, never
-- appended) — this is a "last known state" fact, not an event log; nothing
-- needs the history of every past evaluation, only the most recent one, so
-- there is no `sin90_events` row for this either (H2 deliberately does not
-- widen that append-only log for a fact `AiReadModel`'s own gate is the only
-- reader of).
CREATE TABLE sin90_classify_evals (
    task_id      TEXT PRIMARY KEY REFERENCES sin90_tasks(id),
    evaluated_at TEXT NOT NULL
);

-- M5 (T5.7.2 review round 2): `Sin90Store::list_rejected_ops` filters
-- `sin90_proposal_rejections` by `capability_source` and orders by
-- `rejected_at` — both now covered by one index instead of the full-table
-- scan + sort `idx_sin90_proposal_rejections_proposal` (keyed on
-- `proposal_id`, a different access pattern) does nothing for.
CREATE INDEX idx_sin90_proposal_rejections_capability
    ON sin90_proposal_rejections(capability_source, rejected_at);

-- N-H1 (T5.7.2 review round 2, follow-up): persist a task's 待定 parking
-- provenance ON THE TASK ROW ITSELF, instead of re-deriving it every read via
-- a JOIN back through `sin90_ai_calls`/`sin90_proposals`/`json_each(ops)`
-- (`store/repo.rs`'s `load_task_triage_via_classify`) or a `sin90_events`
-- lookup for "when did this task enter 待定" (`store/ai_port.rs`'s
-- `triage_retry_gate_sql`). That derivation broke the moment a task's id
-- changed underneath it: `CarryOverTask` mints a brand-NEW `sin90_tasks.id`
-- for the carried row (only `direction_id` is copied, never the OLD task's
-- provenance), so the JOIN keyed on the NEW id found nothing — a task
-- fallback-classified into 待定, then carried into the next week, could
-- never again be told apart from one a human parked there directly (M6's
-- whole point), and its "entered 待定 at" floor silently fell back to `''`
-- (no `direction_assigned` event exists for the NEW id either), reopening the
-- H2 gate on every single run regardless of any real Direction ever
-- appearing.
--
-- `triage_via`: `'classify'` (AI fallback, T5.2.2 §2 #30) or `'direct'`
-- (a human/automation client filing `AssignTaskDirection(t, 待定)` straight
-- via `POST /proposals`, Q7 "用户的东西不覆盖") — written by
-- `Sin90Op::AssignTaskDirection`'s apply whenever its TARGET is the reserved
-- 待定 id, from the CURRENT proposal's own resolved `capability_source`
-- (same `"classify"`/`"direct"` vocabulary `reject_proposal` already uses),
-- and cleared back to `NULL` the moment that same op moves the task OUT of
-- 待定 to a real Direction (A3's one-way carve-out — 待定 parking never
-- survives past that point, so neither should its provenance). `NULL` for
-- every task that has never been 待定-parked at all (the common case).
--
-- `triage_entered_at`: when the task most recently entered 待定 — written by
-- the SAME `AssignTaskDirection` apply, at the SAME time as `triage_via`;
-- the H2 gate's floor once no `sin90_classify_evals` row exists yet for this
-- task_id. `CarryOverTask`'s apply copies BOTH columns verbatim from the
-- source task row to the new one, so a carried-over triage task keeps its
-- provenance and entry time under its new id (fixing this migration's own
-- motivating bug).
-- L2 (T5.7.2 review round 3): a `CHECK` constraint pinning the column to
-- exactly the two-value vocabulary this migration's own doc (and
-- `reject_proposal`'s `capability_source`) already promises — SQLite accepts
-- a `CHECK` on `ALTER TABLE ADD COLUMN` (verified against 3.51.0), and it
-- never fires against the existing rows this ADD COLUMN back-fills with
-- `NULL`: `NULL IN ('classify', 'direct')` evaluates to `NULL`, and a CHECK
-- only rejects an outright `FALSE`, never a `NULL` — the same reasoning
-- `sqlite3 :memory:` was used to confirm before landing this. Anything a
-- future write path attempts INSTEAD of `'classify'`/`'direct'`/`NULL` now
-- fails at the SQLite layer, not silently.
ALTER TABLE sin90_tasks ADD COLUMN triage_via TEXT NULL
    CHECK (triage_via IN ('classify', 'direct'));
ALTER TABLE sin90_tasks ADD COLUMN triage_entered_at TEXT NULL;

-- Backfill for any row that reached 待定 BEFORE this migration ran (an
-- existing deployment migrating forward from 0014) — one-time, using the
-- exact EXISTS/event-lookup this migration retires as the read path. New
-- rows going forward are stamped directly by `AssignTaskDirection`'s apply
-- (and copied verbatim by `CarryOverTask`'s apply, `store/repo.rs`); this
-- UPDATE only ever needs to run once, against whatever `sin90_tasks` rows
-- already sit in 待定 at migration time.
--
-- M1 (T5.7.2 review round 3): the naive version of this backfill (join
-- straight off `sin90_tasks.id`) missed every row that reached 待定 via a
-- classify assignment, was THEN carried over one or more times BEFORE this
-- migration ever ran (so the pre-N-H1 `CarryOverTask` apply — the one this
-- migration's own columns did not exist for yet — never had `triage_via`/
-- `triage_entered_at` to copy), and is sitting in 待定 today under a BRAND
-- NEW id its `carried_from` chain leads back through. The `assign_task_
-- direction` op and the `direction_assigned` event were both recorded
-- against the ORIGINAL id, not the current one, so a join keyed only on
-- `sin90_tasks.id` found nothing and silently backfilled every such row as
-- `'direct'` with no `triage_entered_at` at all — indistinguishable from a
-- task that was never AI-classified, defeating both A3's carve-out and the
-- H2 retry gate for it, exactly like N-H1's own motivating bug, just one
-- `CarryOverTask` hop later. Fixed by walking the WHOLE `carried_from`
-- chain (a recursive CTE, `ancestry`: every task_id currently in 待定 paired
-- with itself and every ancestor reachable by following `carried_from`
-- backwards) and checking/deriving from ANY ancestor in that chain, not
-- just the row's own id.
WITH RECURSIVE ancestry(task_id, ancestor_id) AS (
    SELECT id, id FROM sin90_tasks WHERE direction_id = 'sin90-triage'
    UNION ALL
    SELECT ancestry.task_id, t.carried_from
    FROM ancestry
    JOIN sin90_tasks t ON t.id = ancestry.ancestor_id
    WHERE t.carried_from IS NOT NULL
)
UPDATE sin90_tasks
SET triage_via = 'classify'
WHERE direction_id = 'sin90-triage'
  AND EXISTS (
      SELECT 1 FROM ancestry a
      JOIN sin90_ai_calls c ON c.task_kind = 'classify' AND c.ok = 1
      JOIN sin90_proposals p ON p.id = c.proposal_id AND p.status = 'applied'
      JOIN json_each(p.ops) je
      WHERE a.task_id = sin90_tasks.id
        AND json_extract(je.value, '$.op') = 'assign_task_direction'
        AND json_extract(je.value, '$.task_id') = a.ancestor_id
        AND json_extract(je.value, '$.direction_id') = 'sin90-triage'
  );

UPDATE sin90_tasks
SET triage_via = 'direct'
WHERE direction_id = 'sin90-triage' AND triage_via IS NULL;

-- M1: same ancestry walk for `triage_entered_at` — `MAX(at)` across every
-- `direction_assigned`-into-待定 event found anywhere in the chain (self
-- included), since only the id a direct/classify assignment was actually
-- recorded against ever gets one; every purely-carried id in between has
-- only a `created`/`transitioned` event, never `direction_assigned`.
WITH RECURSIVE ancestry(task_id, ancestor_id) AS (
    SELECT id, id FROM sin90_tasks WHERE direction_id = 'sin90-triage'
    UNION ALL
    SELECT ancestry.task_id, t.carried_from
    FROM ancestry
    JOIN sin90_tasks t ON t.id = ancestry.ancestor_id
    WHERE t.carried_from IS NOT NULL
)
UPDATE sin90_tasks
SET triage_entered_at = (
    SELECT MAX(e.at) FROM ancestry a
    JOIN sin90_events e
        ON e.entity = 'task' AND e.entity_id = a.ancestor_id AND e.kind = 'direction_assigned'
        AND json_extract(e.payload, '$.direction_id') = 'sin90-triage'
    WHERE a.task_id = sin90_tasks.id
)
WHERE direction_id = 'sin90-triage';

-- Takes migration slot 0015, the next free one after 0014
-- (`outbox_migrations_are_contiguous_no_gaps`, src/store/repo.rs).
