CREATE TABLE sin90_task_priorities (
    task_id TEXT PRIMARY KEY REFERENCES sin90_tasks(id) ON DELETE CASCADE,
    ai_quadrant TEXT CHECK (ai_quadrant IS NULL OR ai_quadrant IN ('do_first','schedule','delegate','eliminate')),
    ai_importance TEXT CHECK (ai_importance IS NULL OR ai_importance IN ('high','low')),
    ai_urgency TEXT CHECK (ai_urgency IS NULL OR ai_urgency IN ('high','low')),
    ai_rationale TEXT,
    ai_evaluated_at TEXT,
    override_quadrant TEXT CHECK (override_quadrant IS NULL OR override_quadrant IN ('do_first','schedule','delegate','eliminate')),
    override_rationale TEXT,
    updated_at TEXT NOT NULL
);

CREATE TABLE sin90_completion_notes (
    task_id TEXT PRIMARY KEY REFERENCES sin90_tasks(id) ON DELETE CASCADE,
    note TEXT NOT NULL,
    updated_at TEXT NOT NULL
);
