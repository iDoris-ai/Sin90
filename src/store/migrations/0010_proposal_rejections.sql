-- 0010 (T5.7.1, design DESIGN-LIFEOS.md §2 #27/#28): the reject route's
-- append-only audit log — "为将来学习用户习惯积累数据" (tasks.md T5.7.1).
--
-- A SEPARATE table, not columns bolted onto `sin90_proposals` (§2 #28's own
-- reasoning): a rejection is a fact about ONE decision made about a
-- proposal, not a mutable property of the proposal row itself. Keeping it
-- append-only means every existing `sin90_proposals` reader (`row_to_proposal`,
-- `list_pending_proposals`, the whole AI dedup path) needs zero changes
-- beyond the `status` value it already knew how to store (`rejected`, an
-- existing `ProposalStatus` variant — `core/types.rs:125` — that no code
-- path could reach until this migration's route landed).
--
-- Takes migration slot 0010, the next free one after 0009
-- (`outbox_migrations_are_contiguous_no_gaps`, src/store/repo.rs).
-- Opus 2026-09-2x review (T5.7.1) M1: `proposal_source` added (copied
-- verbatim from `sin90_proposals.source` at reject time) — a SECOND,
-- independent axis from `capability_source`. `capability_source` is
-- "which `/ai/*` capability's run produced this proposal, if any" (derived
-- from `sin90_ai_calls`); `proposal_source` is "what the proposal itself
-- self-reports as its author" (`Sin90Proposal.source`,
-- `local_brain`/`executive`/`rule`) — the SAME field `submit_proposal`
-- always required and stored, just copied forward into this log instead of
-- making a reader join back to a `sin90_proposals` row that may, per this
-- file's OWN precondition note below, still exist unmodified but need not
-- be joined to answer this question. The `capability_source` fallback for
-- "no matching `sin90_ai_calls` row" is also renamed `manual` -> `direct`
-- here (M1): the automation key can submit a proposal directly too, not
-- just a human, so "manual" overclaimed who did it.
--
-- Opus review M2 — precondition this whole design leans on: `sin90_proposals`
-- rows are NEVER deleted and `ops` is NEVER rewritten once a proposal exists
-- (submit is create-or-idempotent-replay-only, `submit_proposal`'s own doc;
-- accept/reject only ever flip `status`+`decided_at`+`result`). This table's
-- `ops_summary`/`rationale`/`proposed_at` are therefore safely DERIVED at
-- reject time from a `sin90_proposals` row that will never move out from
-- under them. If a future retention/cleanup pass ever needs to delete OLD
-- `sin90_proposals` rows, it must snapshot their `ops` into this table FIRST
-- (e.g. widen `ops_summary` into a full ops copy) — deleting the source row
-- out from under an already-written rejection log entry is fine (this row
-- never re-reads `sin90_proposals`), but deleting it BEFORE a pending
-- rejection could compute `ops_summary` would leave that reject with nothing
-- to summarize.
CREATE TABLE sin90_proposal_rejections (
    id                 TEXT PRIMARY KEY,
    proposal_id        TEXT NOT NULL REFERENCES sin90_proposals(id),
    -- classify | summarize | propose | direct (no `sin90_ai_calls` row for
    -- this proposal_id — a human/automation client submitted it directly via
    -- `POST /proposals`, design §7.1's "not gated to the AI path" op doc).
    capability_source  TEXT NOT NULL,
    -- Verbatim copy of `sin90_proposals.source` at reject time: local_brain |
    -- executive | rule (`core::ProposalSource`'s wire values).
    proposal_source    TEXT NOT NULL,
    -- Compact "op_kind x count" summary, e.g. "assign_task_direction x1" —
    -- NOT the raw ops JSON (already queryable via `sin90_proposals.ops` for
    -- the same id; duplicating it here would drift the moment `Sin90Op`
    -- grows a new variant's own summary rule).
    ops_summary        TEXT NOT NULL,
    -- The proposal's own `rationale` at reject time ("AI 给的理由").
    rationale          TEXT,
    -- Optional caller-supplied reason (`POST .../reject {"reason"?}`).
    reason             TEXT,
    proposed_at        TEXT NOT NULL,  -- mirrors sin90_proposals.created_at
    rejected_at        TEXT NOT NULL
);
CREATE INDEX idx_sin90_proposal_rejections_proposal ON sin90_proposal_rejections(proposal_id);
