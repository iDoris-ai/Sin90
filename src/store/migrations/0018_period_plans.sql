ALTER TABLE sin90_directions ADD COLUMN goal_3y TEXT NULL;
ALTER TABLE sin90_directions ADD COLUMN goal_from_year INTEGER NULL;

CREATE TABLE sin90_period_plans (
    id TEXT PRIMARY KEY,
    direction_id TEXT NOT NULL REFERENCES sin90_directions(id),
    kind TEXT NOT NULL CHECK (kind IN ('year', 'half', 'quarter', 'month')),
    period TEXT NOT NULL,
    parent_id TEXT NULL REFERENCES sin90_period_plans(id),
    target TEXT NOT NULL,
    metric TEXT NOT NULL CHECK (metric IN ('none', 'count', 'minutes')),
    metric_target INTEGER NULL,
    routine_id TEXT NULL REFERENCES sin90_routines(id),
    source TEXT NOT NULL CHECK (source IN ('ai', 'user')),
    status TEXT NOT NULL CHECK (status IN ('draft', 'active', 'done', 'missed', 'dropped')),
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    UNIQUE(direction_id, kind, period)
);
CREATE INDEX sin90_period_plans_parent ON sin90_period_plans(parent_id);
