//! `GET /review/weekly/draft?week=YYYY-Www` (T4.3.1, spec.md M4, design §M4
//! acceptance: "周复盘草稿里的小时数只来自事件回放，偷偷改表不影响它").
//!
//! Every number here comes from replaying `sin90_events` for the ISO week's
//! `[start, end)` window ([`crate::core::iso_week_bounds`]) — never from
//! `sin90_schedule_blocks` / `sin90_tasks` / `sin90_routines`, which are
//! mutable and can drift from what actually happened. Two exceptions, both
//! still pure event reads (never a mutable-table join):
//!
//! - `by_area` needs a direction's `area_id`, which the `block.transitioned`
//!   event does NOT carry (only `direction_id`/`direction_title` are
//!   snapshotted onto it, see `attention.rs`'s doc). `area_id` is set once,
//!   at `direction.created`, and never changes after (no direction-update
//!   route exists in this codebase — `POST /directions` is create-only), so
//!   replaying `direction.created` events for a `direction_id -> area_id`
//!   map is exactly as safe as the `direction_title` snapshot the block
//!   event already relies on: both are immutable-at-creation facts read
//!   from the append-only log, not from `sin90_directions`.
//! - `by_direction` is literally [`crate::store::Sin90Store::attention`]'s
//!   own output (reused, not reimplemented, per the task's instruction not
//!   to duplicate a differently-sourced hour count), reshaped to the wire's
//!   `{direction_id, minutes}` pair.
//!
//! `routines[].completed` is a hardcoded `0`: nothing in this codebase links
//! a completed `ScheduleBlock` back to the `Routine` that (maybe) generated
//! it — `sin90_schedule_blocks` has no `routine_id` column, and
//! `routine.fired`'s payload (`{routine_id, fire_id, scheduled_for,
//! trigger}`) carries no block/task reference either (T3.2.2). Guessing an
//! association (e.g. "the next block on the same direction") would be
//! exactly the kind of table-joining/fabrication this task explicitly rules
//! out, so it is left at 0 rather than invented. See task report for T4.3.1.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use sqlx::Row;

use crate::store::{Result, Sin90Store, StoreError};

/// One direction's realized minutes over the week — `by_direction`'s wire
/// shape (`{direction_id, minutes}`), reshaped from [`crate::store::AttentionRow`] (which
/// additionally carries a `direction_title` snapshot this endpoint's
/// documented response shape does not ask for).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirectionMinutes {
    pub direction_id: String,
    pub minutes: i64,
}

/// One area's realized minutes over the week — `by_area`'s wire shape
/// (spec.md M4: `by_area:[{area_id,minutes}]`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AreaMinutes {
    pub area_id: String,
    pub minutes: i64,
}

/// One Routine's fired/completed counts over the week — `routines`' wire
/// shape (spec.md M4: `routines:[{routine_id, fired, completed}]`).
/// `completed` is always `0` today — see this module's doc.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoutineDraftRow {
    pub routine_id: String,
    pub fired: i64,
    pub completed: i64,
}

/// `GET /review/weekly/draft?week=YYYY-Www`'s full response (spec.md M4).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WeeklyDraft {
    pub week: String,
    pub by_area: Vec<AreaMinutes>,
    pub by_direction: Vec<DirectionMinutes>,
    pub tasks_done: i64,
    pub routines: Vec<RoutineDraftRow>,
}

impl Sin90Store {
    /// Pure event replay for `iso_week` (`YYYY-Www`). `Err(StoreError::Invalid)`
    /// for a malformed week label — never a silent empty draft (same posture
    /// `attention`'s HTTP handler enforces for its `start`/`end` query, and
    /// `week_attention`'s 404 enforces for an unknown week id).
    pub async fn weekly_draft(&self, iso_week: &str) -> Result<WeeklyDraft> {
        let (start, end) = crate::core::iso_week_bounds(iso_week).ok_or_else(|| {
            StoreError::Invalid(format!(
                "week must be an ISO-8601 week like 2026-W39, got {iso_week:?}"
            ))
        })?;
        // Canonicalized label for the response's `week` field (same
        // lowercase-w-in/uppercase-out convention `create_week` and
        // `create_review`'s weekly period already guarantee).
        let week = crate::core::canonical_iso_week(iso_week)
            .expect("iso_week_bounds already validated this parses");

        // by_direction: reuse `attention`'s own replay verbatim — this is
        // the SAME query `GET /attention` and `week_attention` run, not a
        // re-derived one.
        let attention_rows = self.attention(&start, &end).await?;
        let by_direction: Vec<DirectionMinutes> = attention_rows
            .iter()
            .map(|r| DirectionMinutes {
                direction_id: r.direction_id.clone(),
                minutes: r.actual_min,
            })
            .collect();

        // by_area: fold `by_direction` through the direction->area snapshot
        // (itself pure `sin90_events` replay — see module doc).
        let area_of = self.direction_area_snapshot().await?;
        let mut area_totals: BTreeMap<String, i64> = BTreeMap::new();
        for row in &attention_rows {
            let area_id = if row.direction_id.is_empty() {
                String::new()
            } else {
                area_of.get(&row.direction_id).cloned().unwrap_or_default()
            };
            *area_totals.entry(area_id).or_insert(0) += row.actual_min;
        }
        let by_area: Vec<AreaMinutes> = area_totals
            .into_iter()
            .map(|(area_id, minutes)| AreaMinutes { area_id, minutes })
            .collect();

        let tasks_done: i64 = sqlx::query(
            "SELECT COUNT(*) AS n FROM sin90_events
             WHERE entity = 'task' AND kind = 'transitioned' AND to_state = 'done'
               AND at >= ? AND at < ?",
        )
        .bind(&start)
        .bind(&end)
        .fetch_one(self.pool())
        .await?
        .get("n");

        let routine_rows = sqlx::query(
            "SELECT entity_id AS routine_id,
                    CAST(COUNT(*) AS INTEGER) AS fired
             FROM sin90_events
             WHERE entity = 'routine' AND kind = 'fired'
               AND at >= ? AND at < ?
             GROUP BY entity_id
             ORDER BY entity_id",
        )
        .bind(&start)
        .bind(&end)
        .fetch_all(self.pool())
        .await?;
        let routines: Vec<RoutineDraftRow> = routine_rows
            .into_iter()
            .map(|r| RoutineDraftRow {
                routine_id: r.get("routine_id"),
                fired: r.get("fired"),
                // Always 0 — see module doc: nothing in the event log links
                // a completed block back to a Routine.
                completed: 0,
            })
            .collect();

        Ok(WeeklyDraft {
            week,
            by_area,
            by_direction,
            tasks_done,
            routines,
        })
    }

    /// `direction_id -> area_id` snapshot, replayed purely from
    /// `direction.created` events (never `sin90_directions`, which is
    /// mutable — though for THIS field it wouldn't matter, since a
    /// Direction's `area_id` is set once at creation and no update route
    /// ever changes it; the replay is still preferred over the table for the
    /// same "never join a mutable table" discipline `attention` and
    /// `week_attention` already follow, so this endpoint has exactly one
    /// sourcing rule, not two). A direction created with no area (`area_id:
    /// null`) is absent from the map; callers treat that the same as "no
    /// area" (`""`), matching [`crate::store::AttentionRow`]'s own "no direction" bucket.
    async fn direction_area_snapshot(&self) -> Result<std::collections::HashMap<String, String>> {
        let rows = sqlx::query(
            "SELECT entity_id AS direction_id,
                    json_extract(payload,'$.area_id') AS area_id
             FROM sin90_events
             WHERE entity = 'direction' AND kind = 'created'",
        )
        .fetch_all(self.pool())
        .await?;
        Ok(rows
            .into_iter()
            .filter_map(|r| {
                let direction_id: String = r.get("direction_id");
                let area_id: Option<String> = r.get("area_id");
                area_id.map(|a| (direction_id, a))
            })
            .collect())
    }
}

#[cfg(test)]
mod weekly_draft_tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::core::{
        Energy, FireTrigger, NewRoutine, RoutineKind, ScheduleBlockStatus, TaskKind, TaskStatus,
    };
    use crate::store::test_hooks;

    async fn new_store() -> Sin90Store {
        Sin90Store::open_memory().await.unwrap()
    }

    /// Create a direction (optionally under an area) and hand back its id —
    /// tiny fixture helper, this module's own (not shared with
    /// `routine_tests`/`review_tests`, to keep this file self-contained).
    async fn direction(store: &Sin90Store, area_id: Option<&str>) -> String {
        store
            .create_direction("d", "this-quarter", area_id)
            .await
            .unwrap()
            .id
    }

    /// Create a block on `direction_id`, drive it `planned -> started ->
    /// completed`, then backdate the resulting `block.transitioned` event's
    /// `at` to `at` via the raw-SQL test hook — the only way to put a
    /// completed block on a controlled day instead of "whenever the test
    /// happened to run" (mirrors `set_task_created_at`'s doc).
    async fn complete_block_at(
        store: &Sin90Store,
        direction_id: &str,
        minutes: u32,
        at: &str,
    ) -> String {
        let block = store
            .create_block(Some(direction_id), None, minutes)
            .await
            .unwrap();
        store
            .transition_block(&block.id, ScheduleBlockStatus::Started)
            .await
            .unwrap();
        store
            .transition_block(&block.id, ScheduleBlockStatus::Completed)
            .await
            .unwrap();
        test_hooks::set_last_event_at(store, "block", &block.id, at)
            .await
            .unwrap();
        block.id
    }

    /// Create a task, drive it `backlog -> planned -> in_progress -> done`,
    /// then backdate the `done` transition's event `at` (same technique as
    /// `complete_block_at`).
    async fn complete_task_at(store: &Sin90Store, at: &str) -> String {
        let task = store
            .create_task("t", None, None, TaskKind::Other, Energy::Mid, None)
            .await
            .unwrap();
        for to in [
            TaskStatus::Planned,
            TaskStatus::InProgress,
            TaskStatus::Done,
        ] {
            store.transition_task(&task.id, to).await.unwrap();
        }
        test_hooks::set_last_event_at(store, "task", &task.id, at)
            .await
            .unwrap();
        task.id
    }

    async fn routine(store: &Sin90Store, title: &str) -> String {
        store
            .create_routine(&NewRoutine {
                title: title.to_string(),
                area_id: None,
                direction_id: None,
                kind: RoutineKind::Other,
                cron: "0 7 * * MON".to_string(),
                tz: None,
                target_count: None,
                target_minutes: None,
            })
            .await
            .unwrap()
            .id
    }

    /// Fire `routine_id` once, at `at` — `record_routine_fire` always
    /// stamps `now`, so this backdates the same way the block/task helpers
    /// do.
    async fn fire_routine_at(store: &Sin90Store, routine_id: &str, fire_id: &str, at: &str) {
        store
            .record_routine_fire(
                fire_id,
                &format!("routine.{routine_id}"),
                at,
                FireTrigger::Tick,
            )
            .await
            .unwrap();
        test_hooks::set_last_event_at(store, "routine", routine_id, at)
            .await
            .unwrap();
    }

    // ----- invalid week -> Invalid (400 at the HTTP layer) -------------------

    #[tokio::test]
    async fn weekly_draft_rejects_a_malformed_week_label() {
        let store = new_store().await;
        for bad in ["2026-W99", "2026-W00", "garbage", "2026-39", ""] {
            let err = store.weekly_draft(bad).await.unwrap_err();
            assert!(matches!(err, StoreError::Invalid(_)), "{bad:?}: {err:?}");
        }
    }

    // ----- empty week: zeroed but structurally complete -----------------------

    #[tokio::test]
    async fn weekly_draft_for_an_empty_week_is_all_zero_with_full_structure() {
        let store = new_store().await;
        let draft = store.weekly_draft("2026-W39").await.unwrap();
        assert_eq!(draft.week, "2026-W39");
        assert_eq!(draft.by_area, vec![]);
        assert_eq!(draft.by_direction, vec![]);
        assert_eq!(draft.tasks_done, 0);
        assert_eq!(draft.routines, vec![]);
    }

    // ----- fixed fixture: exact hour math across areas/directions/weeks ------

    /// The task's required fixture: two areas, three directions (one with no
    /// area), several completed blocks — some inside the target week, some
    /// in the week before/after — plus tasks done and routines fired both
    /// inside and outside the window. Every expected number is hand-computed
    /// here, not derived from the code under test.
    #[tokio::test]
    async fn weekly_draft_hours_match_hand_computed_fixture_across_two_weeks() {
        let store = new_store().await;
        let area_work = store.create_area("Work").await.unwrap().id;
        let area_health = store.create_area("Health").await.unwrap().id;
        let dir_coding = direction(&store, Some(&area_work)).await;
        let dir_writing = direction(&store, Some(&area_work)).await;
        let dir_run = direction(&store, Some(&area_health)).await;
        let dir_no_area = direction(&store, None).await;

        // Target week: 2026-W39 = 2026-09-21T00:00:00Z .. 2026-09-28T00:00:00Z.
        let in_week = "2026-09-24T10:00:00Z";
        let in_week_2 = "2026-09-22T08:00:00Z";
        // Adjacent weeks — must NOT be counted.
        let prev_week = "2026-09-20T23:59:59Z"; // 2026-W38
        let next_week = "2026-09-28T00:00:01Z"; // 2026-W40 (>= end)

        complete_block_at(&store, &dir_coding, 120, in_week).await; // Work: 120
        complete_block_at(&store, &dir_writing, 30, in_week_2).await; // Work: +30 = 150
        complete_block_at(&store, &dir_run, 45, in_week).await; // Health: 45
        complete_block_at(&store, &dir_no_area, 15, in_week).await; // no-area direction: 15
        complete_block_at(&store, &dir_coding, 999, prev_week).await; // excluded
        complete_block_at(&store, &dir_coding, 999, next_week).await; // excluded

        let draft = store.weekly_draft("2026-w39").await.unwrap();
        assert_eq!(draft.week, "2026-W39");

        let mut by_area = draft.by_area.clone();
        by_area.sort_by(|a, b| a.area_id.cmp(&b.area_id));
        let mut expected_area = vec![
            AreaMinutes {
                area_id: area_work.clone(),
                minutes: 150,
            },
            AreaMinutes {
                area_id: area_health.clone(),
                minutes: 45,
            },
            AreaMinutes {
                area_id: String::new(),
                minutes: 15,
            },
        ];
        expected_area.sort_by(|a, b| a.area_id.cmp(&b.area_id));
        assert_eq!(by_area, expected_area);

        let mut by_direction = draft.by_direction.clone();
        by_direction.sort_by(|a, b| a.direction_id.cmp(&b.direction_id));
        let mut expected_direction = vec![
            DirectionMinutes {
                direction_id: dir_coding.clone(),
                minutes: 120,
            },
            DirectionMinutes {
                direction_id: dir_writing.clone(),
                minutes: 30,
            },
            DirectionMinutes {
                direction_id: dir_run.clone(),
                minutes: 45,
            },
            DirectionMinutes {
                direction_id: dir_no_area.clone(),
                minutes: 15,
            },
        ];
        expected_direction.sort_by(|a, b| a.direction_id.cmp(&b.direction_id));
        assert_eq!(by_direction, expected_direction);

        // tasks_done: two inside the window, one outside.
        complete_task_at(&store, in_week).await;
        complete_task_at(&store, in_week_2).await;
        complete_task_at(&store, prev_week).await;
        let draft = store.weekly_draft("2026-W39").await.unwrap();
        assert_eq!(draft.tasks_done, 2);

        // routines: two fires for r1 inside the window, one outside; one
        // fire for r2 inside.
        let r1 = routine(&store, "Run").await;
        let r2 = routine(&store, "Write").await;
        fire_routine_at(&store, &r1, "fire-1", in_week).await;
        fire_routine_at(&store, &r1, "fire-2", in_week_2).await;
        fire_routine_at(&store, &r1, "fire-3", prev_week).await;
        fire_routine_at(&store, &r2, "fire-4", in_week).await;
        let draft = store.weekly_draft("2026-W39").await.unwrap();
        let mut routines = draft.routines.clone();
        routines.sort_by(|a, b| a.routine_id.cmp(&b.routine_id));
        let mut expected_routines = vec![
            RoutineDraftRow {
                routine_id: r1,
                fired: 2,
                completed: 0,
            },
            RoutineDraftRow {
                routine_id: r2,
                fired: 1,
                completed: 0,
            },
        ];
        expected_routines.sort_by(|a, b| a.routine_id.cmp(&b.routine_id));
        assert_eq!(routines, expected_routines);
    }

    // ----- week boundary: Sunday 23:59:59Z vs. Monday 00:00:00Z --------------

    #[tokio::test]
    async fn weekly_draft_week_boundary_sunday_2359_vs_monday_0000() {
        let store = new_store().await;
        let dir_id = direction(&store, None).await;

        // 2026-W39 = 2026-09-21T00:00:00Z .. 2026-09-28T00:00:00Z.
        complete_block_at(&store, &dir_id, 10, "2026-09-20T23:59:59Z").await; // W38, excluded
        complete_block_at(&store, &dir_id, 20, "2026-09-21T00:00:00Z").await; // W39, included (start inclusive)
        complete_block_at(&store, &dir_id, 40, "2026-09-27T23:59:59Z").await; // W39, included
        complete_block_at(&store, &dir_id, 80, "2026-09-28T00:00:00Z").await; // W40, excluded (end exclusive)

        let draft = store.weekly_draft("2026-W39").await.unwrap();
        assert_eq!(
            draft.by_direction,
            vec![DirectionMinutes {
                direction_id: dir_id.clone(),
                minutes: 60, // 20 + 40 only
            }]
        );

        // Positive control: the excluded 10 and 80 minutes both land in
        // their own adjacent weeks.
        let prev = store.weekly_draft("2026-W38").await.unwrap();
        assert_eq!(
            prev.by_direction,
            vec![DirectionMinutes {
                direction_id: dir_id.clone(),
                minutes: 10,
            }]
        );
        let next = store.weekly_draft("2026-W40").await.unwrap();
        assert_eq!(
            next.by_direction,
            vec![DirectionMinutes {
                direction_id: dir_id,
                minutes: 80,
            }]
        );
    }

    // ----- negative control: editing the mutable tables must not move the numbers --

    /// The acceptance-critical test: bypass every event-producing route and
    /// mutate `sin90_schedule_blocks`/`sin90_tasks` directly via raw SQL
    /// (`test_hooks`) — the draft's numbers must not move at all, because
    /// they never read those tables.
    #[tokio::test]
    async fn weekly_draft_ignores_direct_table_mutation_negative_control() {
        let store = new_store().await;
        let dir_id = direction(&store, None).await;
        let in_week = "2026-09-24T10:00:00Z";
        let block_id = complete_block_at(&store, &dir_id, 60, in_week).await;
        let task_id = complete_task_at(&store, in_week).await;

        let before = store.weekly_draft("2026-W39").await.unwrap();
        assert_eq!(before.tasks_done, 1);
        assert_eq!(
            before.by_direction,
            vec![DirectionMinutes {
                direction_id: dir_id.clone(),
                minutes: 60,
            }]
        );

        // Bypass events entirely: rewrite the block's planned_minutes and
        // flip the task back to backlog directly on the mutable tables.
        test_hooks::set_block_planned_minutes_direct(&store, &block_id, 99999)
            .await
            .unwrap();
        test_hooks::set_task_status_direct(&store, &task_id, "backlog")
            .await
            .unwrap();

        let after = store.weekly_draft("2026-W39").await.unwrap();
        assert_eq!(
            after, before,
            "editing sin90_schedule_blocks/sin90_tasks directly must not change the draft at all"
        );
    }
}
