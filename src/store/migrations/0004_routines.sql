-- M3 (design §2 #6, §3.2, spec.md M3): Routine — a repeating execution
-- template, orthogonal to Rhythm (Rhythm allocates attention across
-- Directions; Routine is "do X on this cron"). Only adds a table — no
-- existing column type changes (design §4.1: "sin90.db 靠迁移升级，不靠重建").
-- `sin90_routine_fires` is deliberately NOT created here: that table belongs
-- to T3.2.2 (fired-receipt dedup), out of this task's scope.

CREATE TABLE sin90_routines (
    id             TEXT PRIMARY KEY,
    area_id        TEXT REFERENCES sin90_areas(id),
    direction_id   TEXT REFERENCES sin90_directions(id),
    title          TEXT NOT NULL,
    kind           TEXT NOT NULL CHECK (kind IN ('deep_work','exercise','review','read','other')),
    cron           TEXT NOT NULL,
    tz             TEXT NOT NULL DEFAULT 'UTC',
    target_count   INTEGER CHECK (target_count IS NULL OR target_count > 0),
    target_minutes INTEGER CHECK (target_minutes IS NULL OR target_minutes > 0),
    status         TEXT NOT NULL CHECK (status IN ('active','paused','retired')),
    created_at     TEXT NOT NULL,
    updated_at     TEXT NOT NULL
);
CREATE INDEX idx_sin90_routine_status    ON sin90_routines(status);
CREATE INDEX idx_sin90_routine_area      ON sin90_routines(area_id);
CREATE INDEX idx_sin90_routine_direction ON sin90_routines(direction_id);

-- `sin90_events.entity` is unconstrained TEXT (see 0001's comment on 0002's
-- 'area' extension) — 'routine' needs no schema change to be a legal value.
