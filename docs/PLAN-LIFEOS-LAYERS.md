# Sin90 · 三层人生系统扩展方案（v0.6）

日期：2026-11-18 · 状态：草案，待一轮评审后冻结
来源：jason 11-18 对 Agent24 桌面 Sin90 页的产品规划（方向稿：https://claude.ai/artifact/W4KYLAAN62CqFwztq5u4BA）与对 v0.5.2 的核查。

## 0. 一句话

Sin90 要能帮一个普通人**分层规划目标**，并靠**反馈和复盘**跟踪下去，AI 在旁边帮忙。
三层：**方向 → 节奏 → 今天**。复盘不是一层，是一个动作。

## 1. 产品规则（jason 已拍板）

1. **方向**：五个人生系统（Area），每个系统写「现在 → 理想」；系统下的方向（Direction）**自带一个三年目标**，三年拆成每年一段。三年目标是方向的产物，不是单独一层。
2. **节奏**：年 → 半年 → 季 → 月 → 周，从粗到细。**AI 从上一层拆出下一层，人可以修改**（AI 拆的不一定符合实际）。原来的 Rhythm（各方向精力百分比）保留，界面上叫「精力分配」。
3. **今天**：v0.5.2 现状不变（必做、下一块、收件箱、结转）。时间块继续是队列，不加具体时刻。
4. **往上传导**：停一周对上层影响很小；**连续 N 周（默认 3）**没达标，才影响月、季、年；年底的结果修正三年目标，并生成下一年那一段。
5. **复盘**：默认**每周日**自动复盘本周（ISO 周，周一到周日，周日是最后一天）。频率、哪一天可改；自动可以关掉，只手动。月底、季末、年底的复盘并进当周那次。提交任务、随手记**不会**触发复盘。
6. **AI 只提建议**：一切 AI 的改动都是 Proposal，人采纳才生效（v0.5.2 已是这样，不变）。
7. **周**：沿用 ISO 8601（周一开始）。

## 2. v0.5.2 已经一致的（不动）

Area（五系统种子包）、Direction（area_id、状态机）、Week / ScheduleBlock（无日期队列）、Routine（cron、次数/分钟目标）、Task、Proposal（`ops: Vec<Sin90Op>`，CAS 应用）、事件日志与 attention 回放、Review（daily / weekly / rhythm，draft → finalized）、AI 三个能力（classify / summarize / propose，默认只用本机模型，执行脑需单独开启）。

## 3. 要改的底层（按切片）

### L1 · 小改与修 bug（先做）

- **方向可编辑**：`PATCH /directions/{id}`，可改 `title`、`status`（按现有状态机合法边）、`target_window`、`area_id`。今天 `POST /directions` 只能建不能改，「人可以改」走不通。发 `direction.updated` 事件，事件里带改后的快照（weekly_draft 依赖的 direction_id → area_id 映射改为按最新的 `direction.created|updated` 回放）。
- **Area 加「现在 / 理想」**：`sin90_areas` 加 `current_state TEXT NULL`、`ideal_state TEXT NULL`；`PATCH /areas/{id}` 可改。
- **例行 ↔ 时间块关联（bug）**：今天 ScheduleBlock 不记它来自哪个 Routine，周复盘草稿里 `routines[].completed` 写死为 0，所以「每周跑 3 次」无法追踪。`sin90_schedule_blocks` 加 `routine_id TEXT NULL REFERENCES sin90_routines(id)`；`POST /schedule-blocks` 可带；routine fire 生成的块自动带上；`block.transitioned` 事件带 `routine_id`；weekly draft 的 `completed` 从事件算。

### L2 · 三年目标与阶段计划（核心）

- **Direction 加三年目标**：`goal_3y TEXT NULL`、`goal_from_year INTEGER NULL`（三年为 from_year..from_year+2）。
- **新表 `sin90_period_plans`**（年 / 半年 / 季 / 月的统一承载，三年目标的「每年一段」就是 kind=year 的三条）：

  | 列 | 说明 |
  |---|---|
  | id | ULID |
  | direction_id | NOT NULL，所属方向 |
  | kind | `year` / `half` / `quarter` / `month` |
  | period | `2026` / `2026-H2` / `2026-Q4` / `2026-11`（规范化校验） |
  | parent_id | 上一层的 plan（year 为 NULL） |
  | target | 文字目标（必填） |
  | metric | `none` / `count` / `minutes`；`metric_target INTEGER NULL` |
  | routine_id | 可选：用哪个 Routine 的完成来计数 |
  | source | `ai` / `user`；人改过就置为 `user` |
  | status | `draft` / `active` / `done` / `missed` / `dropped` |
  | created_at / updated_at | |

  唯一约束 `(direction_id, kind, period)`。周不进这张表：周仍是 `sin90_weeks`，它属于哪个月按 ISO 周的周四所在月份算。
- **路由**：`GET /period-plans?direction_id=&kind=&period=`、`PATCH /period-plans/{id}`（人改，改后 `source=user`）、`POST /period-plans`（人手建）。
- **新 Op**：
  - `SetDirectionGoal { direction_id, goal_3y, from_year }`
  - `UpsertPeriodPlans { direction_id, plans: Vec<NewPeriodPlan> }`：按 `(kind, period)` 新建或覆盖；**`source=user` 的行不被 AI 覆盖**（返回冲突，留给人决定），这是「人可以改」的保障。
- **新 AI 能力 `decompose`**：输入方向 + 三年目标 + 已有计划 + 近期实际（attention），输出一个 `UpsertPeriodPlans` 提案，从年拆到半年、季、月。走现有 ladder（本机模型优先，规则兜底：把上一层的量化目标按时间均分）。路由 `POST /ai/decompose { direction_id, down_to: "month" }`。

### L3 · 进度与往上传导（计算，不存表）

- `GET /progress?direction_id=&at=YYYY-MM-DD` 返回该方向每个 period plan 的：实际值（按 metric 从事件算：`count` = 关联 routine 的已完成块数或该方向完成的任务数；`minutes` = 该方向已完成块的分钟数）、应达值（按时间进度线性）、状态 `on_track` / `behind` / `at_risk`，以及**最近几周的周达标序列**和**连续未达标周数**。
- 设置 `progress.streak_weeks`（默认 3）：连续未达标周数 ≥ 阈值时，所在的月 / 季 plan 状态才变 `at_risk`；不到阈值只标 `behind`，不往上传。
- 纯回放事件，不依赖可变表，与 attention 同一原则。

### L4 · 复盘扩展

- `ReviewKind` 加 `monthly`（period `YYYY-MM`）、`quarterly`（`YYYY-Qn`）、`yearly`（`YYYY`）。
- **自动复盘设置**：`GET/PUT /settings/review { auto: bool, cadence: "weekly"|"biweekly"|"monthly", weekday: 1..7, hour }`，默认 `{auto: true, weekly, weekday: 7 (周日), hour: 18}`。实现为一条系统 Routine（kind=review），改设置即改它的 cron，关掉即 pause。到点 fire：起草当周 weekly；若这周包含月末 / 季末 / 年末，同时起草 monthly / quarterly / yearly。
- `summarize` 扩展到月 / 季 / 年：事实来自 L3 的 progress。**yearly 额外产出一个 `UpsertPeriodPlans` 提案**：根据今年结果修正后两年的 year 段，并生成明年的半年 / 季拆分——这就是「年度结果生成下一年」。
- 复盘的「影响范围」不存：由该次复盘产出的提案里 op 触及的层（today / week / month / quarter / year / goal_3y）推导，给界面显示用。

### L5 · AI 引导定方向（最后做）

对话放在 Agent24 助理里（它有聊天与长期记忆，能「了解你的全部信息」）。Sin90 只需接受一份结构化方案：一个 Proposal 里批量 `CreateDirection`（扩展带 `area_id`、`goal_3y`、`from_year`）+ `UpsertPeriodPlans`。Sin90 侧工作只是让 `CreateDirection` 带上这几个字段，并允许同一提案里后续 op 引用前面新建的方向（提案内临时 id）。Agent24 侧的「定方向」技能另立任务。

## 4. 兼容与迁移

- 全部是**加列 / 加表**，不改既有列类型，不重建表（沿用 DESIGN-LIFEOS §4.1）。迁移从 0016 起连续编号。
- 新事件：`direction.updated`、`area.updated`、`period_plan.created|updated`、`settings.review.updated`。
- 旧客户端不受影响：新增字段都可空，新路由是新增。
- 版本：完成 L1–L4 后发 **v0.6.0**；Agent24 把随包 pin 升到 v0.6.0。

## 5. 切片与验收

| 片 | 内容 | 关键验收 |
|---|---|---|
| L1 | 方向 PATCH、Area 现在/理想、routine_id 关联 | 改方向后 weekly draft 的 by_area 用新 area；routine 生成的块完成后，weekly draft 的 `routines[].completed` 不再是 0 |
| L2a | period_plans 表、路由、两个新 Op | AI 的 Upsert 不覆盖 `source=user` 的行；period 格式非法 400 |
| L2b | `decompose` 能力 | 无模型时规则兜底能从 year 拆到 month；提案经人采纳才落库 |
| L3 | `/progress` 与传导 | 停一周：状态 `behind` 不上传；连续 3 周：月 plan `at_risk`；数字只来自事件回放 |
| L4 | 复盘扩展与自动设置 | 默认周日 18:00 起草周复盘；关掉 auto 后不再自动起草；包含月末的那周同时出 monthly；yearly 产出下一年的 Upsert 提案 |
| L5 | CreateDirection 扩展 + 提案内引用 | 一个提案能建方向并挂上三年目标和年度段 |

每片：`cargo test`、clippy `-D warnings`、fmt；HTTP 层有黑盒测试；一轮评审即合。

## 6. FU（这次不做）

- 日历视图 / 时间块具体时刻。
- 多个目标挂在同一方向（本方案一个方向一个三年目标）。
- 复盘被输入实时触发（jason 明确不要默认开启）。
- 精力分配（Rhythm）与阶段计划的联动。
