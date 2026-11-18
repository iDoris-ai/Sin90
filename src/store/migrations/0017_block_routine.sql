ALTER TABLE sin90_schedule_blocks ADD COLUMN routine_id TEXT NULL REFERENCES sin90_routines(id);
