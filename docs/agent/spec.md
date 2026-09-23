# Sin90 本轮规格（M3–M5 增量）

> 基线数据模型见 [`../DESIGN-LIFEOS.md`](../DESIGN-LIFEOS.md) §1–§4；本文件只写**本轮新增/变更**，落到能建表的精度。
> 与 DESIGN 冲突时以 DESIGN 为准，并先改 DESIGN §2 表（§10 规则）。记录日期：2026-09-23。

## M3

**`sin90_routines`**：`id TEXT PK, area_id TEXT NULL FK, direction_id TEXT NULL FK, title TEXT NOT NULL, kind TEXT NOT NULL
CHECK(kind IN ('deep_work','exercise','review','read','other')), cron TEXT NOT NULL, tz TEXT NOT NULL DEFAULT 'UTC',
target_count INTEGER NULL CHECK(target_count>0), target_minutes INTEGER NULL CHECK(target_minutes>0),
status TEXT NOT NULL CHECK(status IN ('active','paused','retired')), created_at TEXT, updated_at TEXT`。

状态机：`active ⇄ paused`，`active|paused → retired`（终态；retired 之后 update 也被拒，409）。

**cron 规则**：5 段；**星期字段只接受 `*` 与英文缩写（MON..SUN 及范围/列表），拒绝数字** —— Sin90 与内核共用的 cron 0.15 以 1=周日，与 POSIX（0/7=周日、1=周一）不同，数字会静默错一天；名字在两种语义下都无歧义。**日字段与星期字段不能同时非 `*`**：cron 0.15 对两者取 AND（`schedule.rs:117-125`），POSIX 取 OR，`0 7 1 * MON` 两边含义不同。`cron`/`chrono-tz` 版本与 Agent24 `agent24-scheduler` 精确同步。

**迁移编号不预分配、不留空洞**：谁先写谁取当前 max+1；并行分支后合并的一方 rebase 时顺延（sqlx 按版本顺序应用，空洞会让后补的低号迁移乱序跑在已升级的用户库上）。有一条测试钉住编号连续。已用：0004 routines（T3.1.1）、0005 outbox_failed（T3.3.1）。事件：`routine.created|updated|paused|resumed|retired|fired`。

**`sin90_routine_fires`**：`fire_id TEXT PK, routine_id TEXT NOT NULL FK, scheduled_for TEXT NOT NULL, received_at TEXT NOT NULL`。

**outbox**（表已存在：`id, kind, dedup_key, desired JSON, status pending|done, created_at, done_at`；**T3.3.1 迁移后**状态为 `pending|done|failed`，并加 `failure_kind, last_error, attempts, next_attempt_at`）：
- `kind = scheduler.upsert | scheduler.delete | memory.remember`；`dedup_key` 同值只保留一条 pending（新期望覆盖）。
- 内核 key = `routine.<routine_id>`；`desired = {key, spec: {cron, tz}, enabled}`。
- 对账器：pending → 调内核 → done；失败退避（1s 起，×2，上限 5min）；启动全量对账：内核 `list` 返回完整期望状态，与本地 active/paused 集合**逐字段**比较（spec、enabled、system_disabled_reason），`user_suspended` 行不动。

**fired**：`POST /_a24/scheduler/fired`（挂载模式）；头 `X-A24-Fire-Id`（必需）、`X-A24-Schedule-Key`；body `{key, scheduled_for, fired_at}`；
内核语义是**至少一次**（同一次到点的重试共用 `fire_id`）→ 按 `fire_id` 去重、重复投递幂等返回 2xx；未知 key → 200 + 触发一次对账（内核有、本地没有 = 孤儿）。

**Rhythm 路由**：`POST /rhythms {allocations:[{direction_id,pct}]}`（pct 合计 ≤ 100，direction 存在）、`GET /rhythms`、`GET /rhythms/{id}`。

## M4

`sin90_reviews` 加 `body_ref TEXT NULL`、`period TEXT NOT NULL`（daily `YYYY-MM-DD` / weekly `YYYY-Www` / rhythm `<rhythm_id>`）；
`UNIQUE(kind, period)`。状态 `draft → finalized`（终态）。事件 `review.created|updated|finalized`。
Markdown：`<data_dir>/reviews/<kind>/<period>.md`，定稿时原子写，单向。
`GET /review/weekly/draft?week=YYYY-Www` → `{week, by_area:[{area_id,minutes}], by_direction:[…], tasks_done, routines:[{routine_id, fired, completed}]}`，全部由事件回放。

## M5

`ai/` 模块；引擎 `reflex | local | executive`；`sin90_ai_calls` 每次调用一行。新 Op 由 T5.0.1 定稿后补进本节。
内核调用：`_a24/model/complete{messages, response_format?, max_tokens?, complexity?}`（线格式以 Agent24 `docs/design/ME4-S2-model-callback.md` 冻结版为准）。

## 错误处理

- 内核回调错误：`forbidden`（没被授予）→ 该功能降级并记日志，不 panic；`rate_limited`/`busy` → 退避重试；`quota_exceeded`（及 `forbidden`、`invalid_params`）→ outbox 行标 `failed` 并在 `/today` 暴露，Routine 下次变更时重置为 pending。
- HTTP：校验失败 400、actor 不符 403、状态冲突 409、未知 404；错误体 `{code, message}`，不泄露 SQL/路径。
