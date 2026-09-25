-- T3.2.2 (design DESIGN-LIFEOS.md §2 #16, §4.1; spec.md M3 "fired"): the
-- receipt table `POST /_a24/scheduler/fired` dedups against. The kernel's
-- delivery contract is "at least once, same fire_id on every retry of the
-- same due slot" (Agent24 docs/design/ME4-S1-scheduler-callback.md §4.1/
-- §4.2) — Sin90 must not turn a retried delivery into a second
-- `routine.fired` event, so `fire_id` is the PRIMARY KEY: a second INSERT for
-- an already-seen fire_id is rejected by the schema itself, and the handler
-- uses `ON CONFLICT(fire_id) DO NOTHING` to make that idempotent rather than
-- an error.
--
-- Column set matches spec.md M3's `sin90_routine_fires` line
-- (`fire_id, routine_id, scheduled_for, received_at`) plus one addition:
--
--   trigger   the fired body's `trigger` field (`tick` | `run_now`, kernel
--             design doc §5.3's `FiredBody`) — kept as its own column
--             (not buried in the routine.fired event payload only) so a
--             later reconciler/audit can filter fires by trigger source
--             without parsing JSON. CHECK constraint mirrors the kernel's
--             own closed set for this field.
--
-- Takes migration slot 0006, the next free one after 0005
-- (0005_outbox_failed.sql's header explains why T3.3.1 took 0005 instead of
-- the 0006 spec.md pre-allocated for THIS table): no gap is left, and
-- `outbox_migrations_are_contiguous_no_gaps` (src/store/repo.rs) continues to
-- assert the migration directory has none.
--
-- `routine_id` is a plain FK — no ON DELETE behavior is declared because
-- Sin90 never deletes a `sin90_routines` row (retire is a status transition,
-- design §3.2's terminal state, not a DELETE).

CREATE TABLE sin90_routine_fires (
    fire_id       TEXT PRIMARY KEY,
    routine_id    TEXT NOT NULL REFERENCES sin90_routines(id),
    scheduled_for TEXT NOT NULL,
    trigger       TEXT NOT NULL CHECK (trigger IN ('tick', 'run_now')),
    received_at   TEXT NOT NULL
);

-- `/today`'s "today's fired routines" section (T3.2.2) filters by
-- received_at's UTC-day prefix and joins back to routine_id — this index
-- serves both the join and (combined with the leading routine_id) a
-- per-routine fire history lookup.
CREATE INDEX idx_sin90_routine_fires_routine_received
    ON sin90_routine_fires(routine_id, received_at);
