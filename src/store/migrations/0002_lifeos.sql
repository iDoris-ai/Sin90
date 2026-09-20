-- M0: Area (design §2 #1, §3.2) + Task.parent_task_id (design §2 #3, §3.2).
-- Only adds — no existing column type changes, no table rebuilds (design §4.1:
-- "sin90.db 靠迁移升级，不靠重建"). Both new FK columns are nullable, so every
-- existing row remains valid without a backfill.

CREATE TABLE sin90_areas (
    id         TEXT PRIMARY KEY,
    title      TEXT NOT NULL,
    slug       TEXT NOT NULL UNIQUE,
    status     TEXT NOT NULL,
    sort_key   INTEGER NOT NULL DEFAULT 0,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);
CREATE INDEX idx_sin90_area_status ON sin90_areas(status);

ALTER TABLE sin90_directions ADD COLUMN area_id TEXT REFERENCES sin90_areas(id);
CREATE INDEX idx_sin90_dir_area ON sin90_directions(area_id);

-- Project = a task with children (design §2 #3): no separate entity, just a
-- self-reference. Nesting is constrained to exactly one level by application
-- code (a task that already has a parent may not itself become a parent) —
-- see store::repo::create_task and core::proposal's CreateTask validation;
-- SQLite has no portable "max depth" CHECK, so the app enforces it under the
-- same write lock every other invariant here uses.
ALTER TABLE sin90_tasks ADD COLUMN parent_task_id TEXT REFERENCES sin90_tasks(id);
CREATE INDEX idx_sin90_task_parent ON sin90_tasks(parent_task_id);

-- `sin90_events.entity` value domain is TEXT with no CHECK constraint in
-- 0001, so it already accepts 'area' without a schema change — noted here so
-- the extension is documented, not silent (design §4.1 row: "值域扩展").
