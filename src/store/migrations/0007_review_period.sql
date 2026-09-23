-- 0007 (T4.1.1, design DESIGN-LIFEOS.md §2/§3.2/§4.1 M4 patch; spec.md M4):
-- `sin90_reviews` gets a `period` column and a `UNIQUE(kind, period)`
-- constraint — a Review's identity is now "this kind, this period", not
-- "this kind, this week_id" (the pre-M4 shape could only ever anchor a
-- Review to a Week, which has no way to express a daily or rhythm period).
--
--   daily   period = a calendar date, `YYYY-MM-DD` (core::canonical_iso_date)
--   weekly  period = an ISO-8601 week label, `YYYY-Www` (core::canonical_iso_week,
--                     the same format `sin90_weeks.iso_week` already uses)
--   rhythm  period = the referenced `sin90_rhythms.id`
--
-- Backfill rule for pre-existing rows (T4.1.1 review: "既有 reviews 行怎么回
-- 填 period" — checked the actual data source first, per the global rule):
-- grepping this codebase's history turns up NO `INSERT INTO sin90_reviews`
-- anywhere before this task — `POST /reviews` did not exist, and nothing
-- else ever wrote this table. So on every real upgrading db, `sin90_reviews`
-- is guaranteed empty and this backfill is unreachable in practice. It is
-- still implemented defensively, for the one plausible historical shape the
-- pre-M4 schema could have held: a row whose `week_id` points at a real
-- `sin90_weeks` row gets that week's `iso_week` as its `period` (exactly
-- what a `weekly` Review's period would have been, had one ever been
-- written). Any row that does NOT resolve that way (no `week_id`, or a
-- `week_id` pointing at nothing) gets a `'legacy-' || id` placeholder — `id`
-- is this table's own PRIMARY KEY, so it can never collide with itself or
-- with another legacy row, which is what keeps the new `UNIQUE(kind,
-- period)` index below from ever rejecting this migration.
--
-- Takes migration slot 0007, the next free one after 0006
-- (`outbox_migrations_are_contiguous_no_gaps`, src/store/repo.rs, continues
-- to assert the migration directory has no gaps).
--
-- `body_ref` (spec.md M4's other `sin90_reviews` column) is explicitly OUT
-- OF SCOPE here — that belongs to T4.2.1, not this task.

ALTER TABLE sin90_reviews ADD COLUMN period TEXT NOT NULL DEFAULT '';

UPDATE sin90_reviews
SET period = COALESCE(
    (SELECT w.iso_week FROM sin90_weeks w WHERE w.id = sin90_reviews.week_id),
    'legacy-' || sin90_reviews.id
)
WHERE period = '';

CREATE UNIQUE INDEX sin90_reviews_kind_period_uq ON sin90_reviews(kind, period);
