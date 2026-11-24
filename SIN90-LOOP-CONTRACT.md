# Sin90 loop HTTP contract (desktop integration)

Base path is `/api/v1/sin90` when mounted in Agent24, and the root path in standalone mode. Existing task, direction, rhythm, transition and proposal routes remain authoritative; this contract adds a read model and evidence operations only. All paths below are relative to that base.

## Priority and evaluation

`GET /tasks/{task_id}/priority` returns the task's current assessment, including an explicit unevaluated state:

```json
{
  "task_id": "…",
  "evaluation_status": "unevaluated | evaluated",
  "ai": null,
  "human_override": null,
  "effective": null
}
```

When evaluated, the AI assessment is `{ "quadrant": "do_first | schedule | delegate | eliminate", "importance": "high | low", "urgency": "high | low", "rationale": "…", "evaluated_at": "RFC3339" }`. `effective` is the human override when present, otherwise the AI assessment. Both `human_override` and `effective` expose `quadrant` and `rationale`. An evaluation with insufficient evidence remains `unevaluated`; it must not invent a quadrant.

`POST /ai/priority/evaluate` accepts `{ "task_ids": ["…"], "advice": "…" }` (`task_ids` omitted means all current unfinished tasks; `advice` is optional and limited to 2,000 Unicode characters) and requires the human actor key. It reads the active directions, active rhythm and allocations, and current unfinished tasks; it invokes the configured model path. When present, `advice` is included in the real evaluation prompt as user assessment guidance. It guides that evaluation only and is distinct from the per-task priority override; it does not write or replace an override. Overlong advice is rejected with `400 Bad Request`. Response: `{ "evaluated": [<priority objects>], "unevaluated": [{"task_id":"…","reason":"…"}] }`. This is advisory only and never modifies a human override.

`PUT /tasks/{task_id}/priority/override` accepts `{ "quadrant": "do_first | schedule | delegate | eliminate", "rationale": "…" }`; `DELETE` on the same path clears the override. Both require the human actor key and return the updated priority object. AI runs cannot call these mutations.

## Completion evidence

The existing `PATCH /tasks/{task_id}` with `{ "to": "done" }` remains the only completion operation and does not require a note or assessment. The completion event captures the task identity/title and effective quadrant as it stood at completion when available.

`PUT /tasks/{task_id}/completion-note` accepts `{ "note": "…" }` and adds or replaces the human's retrospective note for the single completion event. It may be used after completion; it never creates/copies a task or another completion. Requires a human actor key. `GET /tasks/{task_id}/completion-evidence` returns `{ "task_id":"…", "completed_at":"…", "quadrant": null, "note": null }` (null is truthful missing evidence).

`GET /review/weekly/draft?week=YYYY-Www` retains all existing fields and adds `completed_tasks`, one row per task completion event in the week: `{ "task_id":"…", "title":"…", "completed_at":"…", "quadrant": null, "note": null }`. It reads completion events, not the current task table, and never synthesizes a result when note is absent. A later note update enriches this same row.

## Compatibility and decisions

Existing `/today`, `/tasks`, `/directions`, `/rhythms`, `/reviews`, and proposal acceptance contracts remain available. Missing priority records are returned as `unevaluated`; desktop clients should treat unknown/missing optional fields from older Sin90 modules as a capability downgrade and retain their existing direction/rhythm flow. Plan changes remain proposals: the client must use the existing proposal acceptance flow, and must not mutate plans from a review suggestion before explicit human acceptance.
