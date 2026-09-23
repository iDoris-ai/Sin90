-- One Week row per ISO calendar week (Codex 2026-09-22 review, Medium #11).
-- Fails on a database that already holds duplicate iso_week rows; none exist
-- in any shipped install (Sin90 has not had a release with real data yet).
CREATE UNIQUE INDEX IF NOT EXISTS sin90_weeks_iso_week_uq ON sin90_weeks (iso_week);
