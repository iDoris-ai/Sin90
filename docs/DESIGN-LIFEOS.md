# Sin90 Life OS —— 正式设计文档 v1

> 文档类型：设计裁决（Design Decisions）+ 里程碑拆解
> 输入：[`LIFEOS-DESIGN-INPUT.md`](LIFEOS-DESIGN-INPUT.md)（2026-09-20 用户构想 + GPT 提议）
> 核对对象：Agent24 主仓库 `rust/crates/agent24-sin90{,-store,-os}`（2026-09-20 实读源码，非文档转述）
> 状态：本文冻结数据模型与 M0 范围；M1+ 只给方向，不冻结
> 最后更新：2026-09-20

> **2026-09-23 起，执行状态与 M3–M5 的任务拆分以 [`agent/tasks.md`](agent/tasks.md) 为准**（pilot 规划层，见 [`agent/roadmap.md`](agent/roadmap.md)）。
> 本文仍是数据模型裁决的权威；下面的进度表是 2026-09-20 的快照，不再更新。

## 🔴 进度与待办（2026-09-20 快照，已由 agent/tasks.md 接管）

| 里程碑 | 状态 | commit |
|---|---|---|
| M0 可加载的最基础个人 OS | ✅ DONE | `0d66f24` |
| M1 Capture & Today | ✅ DONE | `4032e82` |
| M2 Work Pack | ✅ DONE | `8056ade` |
| Agent24 真实挂载验证（非编号里程碑，本轮插入的前置任务） | ✅ DONE | `ab66b37` |
| M3 Routine & Rhythm | 🟡 可以开工——挂载验证已解除阻塞 | — |
| M4/M5/M6 | ⬜ 未开始 | — |

**待办（优先级从高到低）**：
1. **Codex 对抗式评审债**：以上四个 commit 全部只经过本地自审（Codex CLI 额度耗尽，2026-09-22 19:18 恢复）。**挂载验证那次修的 actor-key 门禁安全缺陷（`/capture` 等直写路由的"人类/自动化"约束曾经因为读错 header 而完全不生效）优先送审**。
2. **M3 的 cron 幂等对账验收需要 Agent24 PR [#342](https://github.com/iDoris-ai/Agent24/pull/342)（T11 内核侧删除旧模块）先合并**——不合并的话内核里还有旧的 in-process Sin90 占着 `"sin90"` 名字，真实挂载测试会撞名字。Routine 实体/状态机/路由本身不依赖它，可以先做。
3. M4 起没有阻塞，按设计文档本身的顺序做即可。

完整会话记录见协调 Claude 的 `project_todo_2026-09-20` 记忆（本机）。

---

## 0. 一句话

**Sin90 不是从零设计一个 Life OS，是把 Agent24 内核里那份已验证的 Direction/Rhythm/Task/Proposal/Event 实现搬进本仓库、包成进程外包，并在搬家的同一刀里补上 `Area` 与 `Task` 这两处真正缺的东西。GPT 那份提议里超过一半的内容内核已经有了；本文逐条给出裁决，不重复造。**

---

## 1. 内核已有实现的真实边界（实读源码，2026-09-20）

这一节是后面所有裁决的事实基础。**凡是这里写了"已有"的，设计里不得重做。**

### 1.1 实体与状态机（`agent24-sin90/src/types.rs` + `transitions.rs`，共 739 行）

| 实体 | 状态 | 终态 |
|---|---|---|
| `Direction` | `draft / active / paused / achieved / abandoned` | achieved, abandoned |
| `Task` | `backlog / planned / in_progress / done / dropped / carried_over` | done, dropped, carried_over |
| `Week` | `planning / active / reviewing / closed` | closed |
| `ScheduleBlock` | `planned / started / completed / skipped` | completed, skipped |
| `Rhythm` | `active / adjusted / retired`（含 `adjusted → adjusted` 自环） | retired |
| `Review` | `draft / finalized`；`kind = daily / weekly / rhythm` | finalized |
| `Proposal` | `pending / applying / applied / rejected`（`applying` 是 CAS 认领态） | applied, rejected |

每个实体一对 `<entity>_transition_allowed` + `check_<entity>_transition`，落库前强制校验；测试是**全矩阵穷举**（对每个 `(from,to)` 断言 `legal.contains()`），不是抽样。

附属值枚举：`TaskKind{deep_work,admin,meeting,learning,other}`、`Energy{high,mid,low}`、`Alloc{direction_id,pct}`。

### 1.2 存储（`agent24-sin90-store/migrations/0001_sin90.sql`，143 行）

独立 `sin90.db`，**不写内核的 `agent24.db`**。表：
`sin90_directions` / `sin90_weeks` / `sin90_rhythms` / `sin90_tasks` / `sin90_schedule_blocks` / `sin90_reviews` / `sin90_events` / `sin90_proposals` / `sin90_outbox` / `sin90_ai_calls` / `sin90_attention_daily` / `sin90_attention_watermark`。

值得注意的既有设计（都不要动）：
- `sin90_events` 是 `seq INTEGER PRIMARY KEY AUTOINCREMENT` 的 append-only 日志，`payload` **自包含**（回放不 join 可变表）。
- `sin90_attention_daily` 是物化视图，靠 `sin90_attention_watermark` 单行水位保证"增量 == 全量重建"，有测试。
- `sin90_outbox`：需要写内核的副作用（如注册 cron）经 apply 后幂等对账，**不跨库两阶段提交**。
- `idx_sin90_task_carried` 唯一索引：一个 task 最多被 carry-over 一次。

### 1.3 Proposal 门（`agent24-sin90/src/proposal.rs`，691 行）

`Sin90Op` 六个变体：`CreateDirection` / `TransitionTask` / `CreateTasks` / `ReorderTasks` / `AdjustRhythm` / `CarryOverTask`。
`Sin90Proposal{id,status,source,ops,rationale}`，`source ∈ {local_brain, executive, rule}`。
`validate` 是**纯函数**（无 I/O），通过 `ValidationCtx` trait 拿 store 快照；内部 `Working` 视图把同批次内前序 op 的效果叠上去，所以 `[t→InProgress, t→Done]` 通过而 `[t→InProgress, t→InProgress]` 失败。所有输入结构 `deny_unknown_fields`。

### 1.4 API 面（`agent24-sin90-os/src/lib.rs`，530 行）—— 就是七条

```
POST|GET  /directions
POST|GET  /schedule-blocks
PATCH     /schedule-blocks/{id}
POST|GET  /proposals
GET       /proposals/{id}
POST      /proposals/{id}/accept
GET       /attention
```
路由是**相对路径**，命名空间由内核加。事件经 `KernelCtx::events()` 发出，kind 为 `direction.created` / `block.created` / `block.transitioned` / `proposal.submitted` / `proposal.applied`，内核包成 `{type:"module", payload:{module:"sin90", kind, payload}}`。`ctx.events()` 返回 `None`（未授予）时**降级不报错**。

### 1.5 挂载形态（`domain-os.yml` + ADR-029）

`name` 是身份唯一来源，`route_namespace`/`event_module`/`data_dir` 三处由它派生且强校验。今天是 `impl_kind: in_process_crate`。

### 1.6 进程外通道今天到底有什么（ME-3，2026-09-20 收口）

实读 `rust/apps/agent24d/tests/me3f_blackbox.rs`（T9/ME-3f 黑盒验收，已合入 main，commit `164120b`）：内核 spawn 子进程时通过**环境变量**交付四样东西——

| 环境变量 | 含义 |
|---|---|
| `A24_DATA_DIR` | 模块自己的数据目录（`~/.agent24/os/<name>/`） |
| `A24_CALLBACK_SOCK` | 回调通道的 Unix socket 路径（NDJSON / JSON-RPC） |
| `A24_HANDSHAKE_TOKEN` | `initialize` 时必须回报的令牌 |
| `A24_LISTEN_FD` | 内核**已 bind 好的** HTTP listener fd，模块直接 `accept` |

`initialize` 请求体形状（黑盒里的实测形状）：
```json
{"protocol_versions":{"min":1,"max":1000},"module":"<name>",
 "manifest_digest":"sha256:<domain-os.yml 的摘要>",
 "auth_token":"<A24_HANDSHAKE_TOKEN>",
 "capabilities":["events"]}
```
manifest 的 `impl_kind: out_of_process_provider` 必须**同时**带 `spawn: {command, args}`，两个方向都校验（少一个或多一个都拒）。`spawn.command` 不许绝对路径、不许 `..`。

**结论：`docs/STATUS.md` 里那张"等哪几刀"的表已经全部作废，ME-3a→3g 全绿。**

### 1.7 今天还缺的（本仓库 M0 要自己扛的）

`agent24-os-sdk`（Agent24 的 T13）**还不存在**——`rust/crates/` 下没有这个 crate。所以 M0 期间 Sin90 要自己写握手那 ~200 行（NDJSON 单帧上限 1 MiB、版本区间协商、`initialize`）。见 §7 开放问题 2。

---

## 2. 逐条裁决：GPT 提议 vs 内核现状

**表头含义**：采纳 = 新增/照做；改造 = 采纳意图但换实现；拒绝 = 不做，给理由和复审触发条件；已满足 = 内核已有，确认延续不重做。

| # | GPT 提议（`LIFEOS-DESIGN-INPUT.md` §3） | 裁决 | 理由 |
|---|---|---|---|
| 1 | 加 `Area` 层 | **采纳** | 真正缺的一层。用户的"五大人生系统"和"六分法"要落到数据里就必须有一个**无生命周期的永久容器**；`Direction` 带 `target_window`（月/季），撑不起"健康系统"这种永远不结束的东西。Area 一加，Pack 就是数据不是代码。 |
| 2 | 加 `Goal` 层 | **拒绝** | `Direction` 的 `achieved` 终态已经是 Goal 语义。插一层会让用户每次录入做两次分类决策（这是 PARA 类系统最常见的弃用原因），而代价是一整套状态机+路由+事件+迁移。若需要可度量目标，加 `Direction.metric` 一列即可。**复审触发条件**：出现"同一 Direction 下需要并列多个互不相关、完成时间显著不同的可完成目标"的真实用例。 |
| 3 | 加 `Project` 层 | **改造** | 不做独立实体，用 `Task.parent_task_id` 自引用表达（Project = 有子任务的 Task）。理由：Project 与 Task 的生命周期状态几乎完全重合（backlog/planned/in_progress/done/dropped），一条自引用列换掉一整套实体。**复审触发条件**：出现 Project 独有且 Task 上放不下的字段（预算、干系人、里程碑）。 |
| 4 | 加 `Execution` 实体 | **拒绝（已满足）** | `ScheduleBlock` 的 `planned→started→completed/skipped` 就是"实际发生"，`attention` 回放已经从 block 完成事件算 actual。加 `Execution` 是开第二套账，两套账必然对不上。 |
| 5 | 加 `Review` 环节 | **已满足** | `Review{kind: daily/weekly/rhythm, status: draft/finalized}` 内核已有实体与状态机，只缺路由（M4 补）。 |
| 6 | Rhythm 细分 `Routine` | **采纳** | 现有 `Rhythm` 只表达"Direction 间的注意力配额"（`Vec<Alloc>`），表达不了"每周 3 次 ×30 分钟"。`Routine` 是重复发生的执行模板，与 Rhythm 正交，不是它的子类；触发规则是 `cron` + `tz`（IANA 时区名，缺省 `UTC`——cron 本身不含时区，DST 边界必须显式）。M3。 |
| 7 | Rhythm 细分 `ScheduleBlock` | **已满足** | 内核已有，含状态机与三条路由。 |
| 8 | Rhythm 细分 `ReviewCycle` | **拒绝** | 复盘周期 = `Routine{kind: review}` + 已有的 `Review` 实体。第三个对象没有新增表达力。 |
| 9 | Rhythm 细分 `Reminder` | **拒绝** | 提醒是内核 `agent24-scheduler` 的职责。Sin90 只在 `Routine` 上声明触发规则（cron 表达式），经 `sin90_outbox` 幂等对账落到内核。自建 Reminder 实体 = 重写内核已有的调度器。 |
| 10 | Proposal 语义收紧（AI 不直接改状态） | **已满足（但有一处名不副实，见下）** | `sin90_proposals` + CAS 幂等 + 纯函数 validate + 单事务 apply 全都在。语义不改，只随新实体扩 `Sin90Op` 变体。 |
| 11 | Hybrid SQLite/Markdown 存储 | **改造** | 采纳"SQLite 是 operational source of truth"，但收紧 Markdown 的角色：**单向、由行持有指针**（`reviews.body_ref` 指向文件），不做双写、不做双向同步。双向同步是这类系统最大的坑，且没有任何一条 M0-M4 的需求要它。M0 不引入 Markdown。 |
| 12 | 补 Event Log 抽象 | **已满足** | `sin90_events` append-only + 自包含 payload + 水位保证增量==全量重建，且有测试。**硬约束延续**：新增的 Area/Routine 必须同样产事件，重构不得退化成只存快照。 |
| 13 | Domain Logic 不 import Agent24，只有 adapter 知道 | **采纳原则，改造形态** | 见 §4——物理分模块采纳；"standalone 与 Agent24 双轨部署"**拒绝**，它与 T11 验收标准打架。 |
| 14 | M0-M11 里程碑表 | **改造** | GPT 的 M0（"数据模型+SQLite+Event"）内核早就有了，照它排会先把已有能力退化再补回来。重排见 §5。 |
| 15 | outbox 状态扩为 `pending\|done\|failed`（T3.3.1） | **采纳** | `sin90_outbox` 原状态只有 `pending\|done`，表达不了"这条对账意图已经永久失败，不该再重试"。加 `failed` + `failure_kind`/`last_error`/`attempts`/`next_attempt_at` 四列（迁移 `0005_outbox_failed.sql`，纯加列，`status` 本来就没有 CHECK，见 §1.2；编号占 `0005` 而非 spec.md 原分配的 `0006`——T3.2.2 尚未开工，`0005_routine_fires.sql` 还不存在，留空洞会让 T3.2.2 之后落地一个编号更小却更晚出现的迁移，见迁移文件自身注释与 `outbox_migrations_are_contiguous_no_gaps` 测试）。错误分类（spec.md "错误处理"）：**永久**（`forbidden`/`quota_exceeded`/`invalid_params`）→ `failed`，在 `/today` 暴露，Routine 下次变更时重置为 `pending`；**可重试**（`rate_limited`/`busy`/`timeout`/断连/`not_ready`/`draining`）→ 退避后重试，`next_attempt_at` 记录何时可再试。对账落地（T3.3.2）不在本条范围。 |
| 16 | fired 投递去重表 `sin90_routine_fires`（T3.2.2） | **采纳** | 内核调度是**至少一次**投递、同一次到点的重试共用 `fire_id`（Agent24 `docs/design/ME4-S1-scheduler-callback.md` §4.1/§4.2）——Sin90 侧必须按 `fire_id` 幂等去重，否则一次到点的重试会被记成多次 `routine.fired`。表只做去重记录，不做内核那边已有的状态机/重试/退避（那些留在内核的 `schedule_deliveries`，spec.md M3 "fired"）：`fire_id TEXT PRIMARY KEY, routine_id TEXT NOT NULL REFERENCES sin90_routines(id), scheduled_for TEXT NOT NULL, trigger TEXT NOT NULL CHECK(trigger IN ('tick','run_now')), received_at TEXT NOT NULL`（迁移 `0006_routine_fires.sql`，`spec.md` 原文的列集是 `fire_id, routine_id, scheduled_for, received_at`；`trigger` 是本条新加的——投递 body 里本来就带 `trigger`（内核设计文档 §5.3 `FiredBody`），`(routine_id, 来源)` 各自独立去重/审计时用得上，不加就要在 `payload` JSON 里翻找）。`POST /_a24/scheduler/fired` 只在**挂载模式**注册（architecture.md #4：这条路径的可信性来自内核代理剥掉客户端伪造的 `X-A24-*` 头，standalone 模式没有代理这层）；未知 `key`/已 `retired` 的 routine 一律 200 且不写行不发事件，留给对账器（T3.3.2，未做）处理孤儿——这里不反向调内核。 |

### 2.1 一处名不副实，值得单独记一笔

GPT 说"AI 想改状态必须先生成 Proposal"——内核**有这个机制，但今天不是强制的**：`POST /directions` 和 `PATCH /schedule-blocks/{id}` 是直写通道，handler 直接调 store，不过 Proposal 门。区分 AI 与人只靠 `Sin90Proposal.source` 这个**自报字段**，协议层没有任何东西会在 AI 走直写通道时响。

**裁决**：M0 保留现状（直写通道给人类 UI，Proposal 通道给 AI），但在文档里如实写成"约定"而不是"保证"。要不要在协议层强制，列为开放问题（§7.1）。

> 这条遵循 Agent24 的既有工程习惯：**措辞不能比机制强**。

---

## 3. 最终数据模型

### 3.1 对象图

```
Area（永久容器，无生命周期）
 └─ Direction（月/季度方向，有终态）
     ├─ Task ──self──> Task（parent_task_id：Project 就是有子任务的 Task）
     │    └─ 可挂在 Week 下
     └─ ScheduleBlock（计划的执行块 → 实际由事件对账）

横切：
  Rhythm    ── Direction 间的注意力配额（Vec<Alloc>）
  Routine   ── 重复发生的执行模板（M3）
  Week      ── 周容器
  Review    ── daily / weekly / rhythm
  Proposal  ── 所有 AI 变更的唯一入口
  Event     ── 上面每一次状态变更的 append-only 记录（地基，不是可选项）
```

### 3.2 实体清单与核心字段

**新增（本文引入）**

| 实体 | 核心字段 | 状态机 |
|---|---|---|
| `Area` | `id, title, slug, sort_key, status, created_at, updated_at` | `active / archived`（两态，`active→archived→active` 均合法——归档一个人生领域再捡回来是正常的） |
| `Routine`（M3） | `id, area_id?, direction_id?, title, kind(deep_work/exercise/review/read/other), cron, tz(IANA，缺省 UTC), target_count, target_minutes, status` | `active / paused / retired` |

**沿用内核（不改语义）**

`Direction`（+ 新增 `area_id` 可空外键）、`Task`（+ 新增 `parent_task_id` 自引用）、`Week`、`ScheduleBlock`、`Rhythm`、`Review`、`Proposal`、`Event`。

字段级改动只有两处，**都是可空的新列**，对既有数据是无损迁移：
- `sin90_directions.area_id TEXT REFERENCES sin90_areas(id)` —— 可空，未分类的 Direction 仍合法。
- `sin90_tasks.parent_task_id TEXT REFERENCES sin90_tasks(id)` —— 可空；**约束：不允许自引用成环，且只允许一层**（M0 只查"父任务自己不能有父任务"，两行 SQL，比通用环检测便宜且够用；需要多层时再放开）。

### 3.3 `Sin90Op` 的扩展

新增变体（M0）：
```
CreateArea   { title }
CreateTask   { title, direction_id?, parent_task_id?, kind?, energy?, est_minutes? }
```
`CreateTasks{week_id, tasks}` 保留（它是"往某周批量塞任务"，与上面单条创建不是一回事）。

`ValidationCtx` 相应扩两个方法：`area_exists(&self, id) -> bool`、`task_parent(&self, id) -> Option<Option<TaskId>>`（外层 `None` = 任务不存在，内层 `None` = 没有父任务）——这是本次唯一一次加宽这个 trait，加宽是破坏性变更，所以一次加够。

---

## 4. 存储方案

### 4.1 SQLite（`~/.agent24/os/sin90/sin90.db`，唯一 operational source of truth）

沿用内核的 12 张表（见 §1.2），**迁移 `0002_lifeos.sql` 只加不改**：

| 表 | 关键列 | 新/旧 |
|---|---|---|
| `sin90_areas` | `id PK, title, slug UNIQUE, sort_key, status, created_at, updated_at` | **新** |
| `sin90_routines`（M3） | `id PK, area_id FK, direction_id FK, title, kind, cron, tz, target_count, target_minutes, status, created_at, updated_at` | **新** |
| `sin90_directions` | + `area_id TEXT REFERENCES sin90_areas(id)` | 加列 |
| `sin90_tasks` | + `parent_task_id TEXT REFERENCES sin90_tasks(id)`，+ `idx_sin90_task_parent` | 加列 |
| `sin90_events` | `entity` 值域扩 `area` / `routine`；schema 不变 | 值域扩展 |
| `sin90_outbox`（T3.3.1） | `status` 值域扩 `pending\|done\|failed`（无 CHECK，代码约束）；+ `failure_kind TEXT NULL`、`last_error TEXT NULL`、`attempts INTEGER NOT NULL DEFAULT 0`、`next_attempt_at TEXT NULL` | 加列 |
| `sin90_routine_fires`（T3.2.2） | `fire_id PK, routine_id FK, scheduled_for, trigger CHECK(tick\|run_now), received_at` —— 按 `fire_id` 去重内核的至少一次 fired 投递 | **新** |
| 其余 8 张 | 不变 | 旧 |

**不做的事**：不重建表、不改现有列类型、不动 `sin90_attention_*` 的水位语义。用户的 `sin90.db` 靠迁移升级，不靠重建（Agent24 §4.3 既有硬约束）。

### 4.2 Markdown（M4 才引入）

| 用途 | 落法 |
|---|---|
| 复盘正文、年度总结、人生原则、研究笔记、AI Report | `~/.agent24/os/sin90/notes/<yyyy>/<id>.md`，由 `sin90_reviews.body_ref` 单向指向 |
| 结构化数据（Area/Direction/Task/…） | **永不落 Markdown** |

规则三条：
1. **SQLite 是权威，Markdown 是正文附件。** 文件丢了，行还在，只是正文空。
2. **单向。** 不扫描目录反向建行，不做 Obsidian 双向同步。
3. **不依赖文件名承载语义。** 语义在行里。

> 这是对 luneth90/lifeos 与 obsidian-lifeos 的**有意偏离**：它们把 Vault 当 source of truth，代价是每一次查询都要扫文件、每一次并发写都要赌。Sin90 有 SQLite 和事件日志，付不起这个代价也不需要付。

---

## 5. Agent24 集成边界

### 5.1 问题：GPT 的"解耦"与 T11 的验收标准会不会打架

- GPT §3.5：Domain Logic 不 import Agent24，只有 `adapter-agent24/` 知道；**Sin90 可以先 standalone 跑起来**。
- T11 验收：内核删掉 `agent24-sin90-os` 后，装上本仓库产出的包，**七条路由行为不变** —— 即 Sin90 的交付形态是**跑在 agent24d 里的进程外模块**。

**裁决：采纳"分层"，拒绝"双轨部署"。** 分层是源码组织，双轨是产品形态；GPT 把两件事混成了一件。Sin90 只有一个交付形态 = Agent24 的 out-of-process package。standalone 只作为**测试运行模式**存在，不是产品承诺。

### 5.2 模块划分（同一个仓库，物理分开，依赖单向）

```
src/
├── core/                零 Agent24 依赖，零 I/O。实体、状态机、Proposal validate。
│                        = 内核 agent24-sin90 整体搬过来。
│                        判据：Cargo.toml 里 core 那层不出现任何 agent24-* 依赖。
├── store/               SQLite。依赖 core。不知道 HTTP，不知道 Agent24。
│                        = 内核 agent24-sin90-store 整体搬过来。
├── http/                业务路由 handler。依赖 core + store。相对路径。
│                        不知道 Agent24 是谁 —— 它只知道"有人会把请求送进来，
│                        有人会收走我产的事件"，后者是一个本地 trait EventSink。
└── adapter_agent24/     唯一知道 Agent24 的地方。
                         读 A24_DATA_DIR / A24_CALLBACK_SOCK / A24_HANDSHAKE_TOKEN /
                         A24_LISTEN_FD；跑 initialize 握手；把 http 层的 EventSink
                         接到 _a24/events/emit；从 A24_LISTEN_FD accept 起 HTTP。
```

依赖方向严格单向：`core ← store ← http ← adapter_agent24`。反向依赖一条都没有。

**standalone 模式**（`sin90 serve --standalone --port 8099`）：跳过 `adapter_agent24`，用一个丢弃事件的 `EventSink` + 自己 bind 端口。用途只有两个——本地开发调试、集成测试不必起 daemon。**它不是发布形态，不写进 README 的"怎么用"。**

### 5.3 与内核的能力边界（不变）

`kernel_capabilities: [events]`。不要 memory（自己管 `data_dir`），不要 approval（M0 无需审批的副作用）。代码按"句柄可能不在"写：`events()` 拿不到就降级，不失败。

### 5.4 `domain-os.yml` 的改动

```yaml
name: sin90
version: "0.5.0"
route_namespace: /api/v1/sin90
event_module: sin90
data_dir: ~/.agent24/os/sin90/
kernel_capabilities: [events]
impl_kind: out_of_process_provider      # ← 从 in_process_crate 改过来
spawn:
  command: bin/sin90                     # 相对包内路径，不许绝对、不许 ..
  args: ["module"]
```

---

## 6. 里程碑拆解

### M0 —— 可加载的最基础个人 OS ★ 本文重点

**一句话**：把内核那份实现整体搬进本仓库、包成进程外包装回去，七条路由行为不变；同一刀里补上 `Area` 与 `Task` 的完整 CRUD + 状态机路由 + 事件查询，使一个人第一次能只靠 Sin90 完成"分领域 → 定方向 → 建任务 → 做完 → 看到它发生过"这条闭环。

**为什么 M0 不是"从零写一个最小模型"**：GPT 的 M0 只含 Area/Direction/Project/Task/Event/Proposal，比内核今天已经跑着的少了 ScheduleBlock / Rhythm / Week / Review / attention 回放。照它做等于**先把能力退回去再补回来**，中间态还要维护。所以 M0 = 搬家（全量）+ 补两处缺口。

#### M0 交付物

1. 本仓库 `src/{core,store,http,adapter_agent24}` 四层齐备，依赖单向。
2. `cargo build --release` 产出 `bin/sin90`，`domain-os.yml` 为 `out_of_process_provider`。
3. 迁移 `0002_lifeos.sql`：`sin90_areas` 表 + `directions.area_id` + `tasks.parent_task_id`。
4. 路由 = 既有七条 + 新增六条：
   ```
   既有（行为必须不变）
   POST|GET  /directions          POST|GET  /schedule-blocks
   PATCH     /schedule-blocks/{id}  POST|GET  /proposals
   GET       /proposals/{id}      POST      /proposals/{id}/accept
   GET       /attention
   新增
   POST|GET  /areas
   PATCH     /areas/{id}          # body {to: "archived"}
   POST|GET  /tasks               # GET 支持 ?direction_id= / ?area_id= / ?status=
   PATCH     /tasks/{id}          # body {to: <TaskStatus>}，走状态机校验
   GET       /events              # ?entity=&entity_id=&since_seq=&limit=
   ```
5. 新事件 kind：`area.created` / `area.transitioned` / `task.created` / `task.transitioned`。
6. Agent24 主仓库侧（**不在本仓库，是 T11 的另一半**）：删除 `agent24-sin90-os` crate 与 `serve` 处的挂载。

#### M0 验收标准（黑盒，照 T9/ME-3f 的精神）

前置：`cargo build` 一次 daemon，之后**不改内核源码、不重新构建内核**。
`agent24 os install <sin90 包目录>` → 重启 daemon。

| # | 判据 | 期望 |
|---|---|---|
| A1 | 挂载 | `agent24 os` 列出 `sin90 0.5.0 out_of_process_provider enabled` |
| A2 | 代理通 | `GET /api/v1/sin90/directions` → 200（不是 404、不是 503） |
| A3 | **七条不变** | 把 Agent24 仓库里现有的 sin90 路由集成测试，改成打 daemon 的真实端口跑一遍，**逐条断言与内核内挂载时同结果**（含 `attention` 的 planned vs actual 数值） |
| A4 | 建领域 | `POST /areas {"title":"工作"}` → 201，返回 `id` |
| A5 | 建方向 | `POST /directions {"title":"Q4 交付 v0.5","target_window":"2026-Q4","area_id":"<A4 的 id>"}` → 201 |
| A6 | 建任务 | `POST /tasks {"title":"写 M0 设计","direction_id":"<A5 的 id>"}` → 201，`status == "backlog"` |
| A7 | 推进 | `PATCH /tasks/{id} {"to":"in_progress"}` → 200；再 `{"to":"done"}` → 200 |
| A8 | **非法迁移被拒** | `PATCH /tasks/{id} {"to":"backlog"}`（done 是终态）→ 409 + 结构化错误体，**且不产生事件** |
| A9 | 能看见 | `GET /tasks?direction_id=<A5>` 返回含该任务；`GET /areas` 返回含"工作" |
| A10 | **事件记录** | `GET /events?entity=task&entity_id=<id>` 返回**恰好 3 条**：`created` / `transitioned(backlog→in_progress)` / `transitioned(in_progress→done)`，`seq` 严格递增，每条 payload 自包含 |
| A11 | 事件转发 | WS `/api/v1/events` 上收到 `{"type":"module","payload":{"module":"sin90","kind":"task.transitioned",...}}` |
| A12 | 持久化 | 数据落在 `~/.agent24/os/sin90/sin90.db`；`kill` daemon 后重启，A9/A10 结果不变 |
| A13 | 进程清理 | SIGTERM daemon 后，进程组内 `bin/sin90` 子进程集合为空 |

#### M0 的反向判据（判据本身必须先被验过）

Agent24 的既有工程习惯：一条判据"绿"有两种原因，被测的东西是对的、或者**判据根本不会响**。所以每条关键判据配一次故意改坏：

| 故意改坏 | 必须变红的判据 |
|---|---|
| 从 `task_transition_allowed` 里删掉 `(InProgress, Done)` 这条边 | A7 后半 |
| 给 `task_transition_allowed` **加上** `(Done, Backlog)` | A8 |
| `emit("task.transitioned")` 那行注释掉 | A10（3 条变 2 条）、A11 |
| `domain-os.yml` 的 `name` 改成 `sin90x` | A1 必须**挂载失败**，而不是静默挂到 `/api/v1/sin90x` |
| `spawn.command` 写成绝对路径 | 安装/挂载被拒 |
| 迁移里把 `area_id` 写成 `NOT NULL` | 既有 `sin90.db` 升级必须失败得响亮，而不是丢数据 |

A10 那条"恰好 3 条"是有意的**正对照**：一个恒为 0 或恒为空的计数，和一个真的是 0 的计数，看起来一模一样。断言精确条数才能同时抓住"没产事件"和"产重了"。

#### M0 明确不含

Rhythm 的路由（实体和状态机在，但不开 HTTP 面）、Week 路由、Review 路由、`Routine`、Markdown、任何 AI 参与、Goal/Project 实体、`/today` 视图、可视化、`agent24-os-sdk`（自己写握手）。

#### M0 完成后，一个人具体能做什么

打开终端（或任何能发 HTTP 的壳）：建立"工作/健康/学习"三个 Area；在"工作"下开一个"Q4 交付 v0.5"的 Direction；往它下面塞任务；把任务推到 in_progress 再推到 done；随时问"这个任务身上发生过什么"并拿到一条带时间序的事件链；关掉电脑第二天打开，全都还在。

**这就是"可加载的最基础个人 OS"的下限**：数据是自己的、变更有账可查、Agent24 真的把它装上了。

---

### M1 —— Capture & Today

**交付**：`POST /capture`（一行文字进 inbox，不要求分类）+ `GET /today`（今天的 3 件必做 + 90 分钟深度块 + 未完成的 carry-over 候选）。inbox 落成 `direction_id IS NULL` 的 backlog task，不新增实体。
**验收**：一条未分类文字 5 秒内入库并出现在 `/today` 的待分类区；`/today` 的输出纯由查询产生，不落第三张表。
**借鉴来源**：obsidian-lifeos 的 Quick Capture —— 它验证过的那条是"录入的摩擦决定系统会不会被弃用"，不是它的文件布局。

### M2 —— Work Pack（第一个垂直闭环）

**交付**：`Task.parent_task_id` 的子任务树 + Week 路由 + `CarryOverTask` 全链打通 + `ReorderTasks`。
**验收**：一周从 `planning` 开到 `closed`，未完成任务 carry-over 到下周并留链；attention 回放能算出这周 planned vs actual 的偏差。
**为什么第一个垂直模块是 Work 而不是 Wealth/Health**：Work 一次把 Direction/Task/Week/ScheduleBlock/Event/Proposal 全跑一遍，是 Core 的压力测试。

### M3 —— Routine & Rhythm

**交付**：`Routine` 实体 + 路由；Routine 的 cron 经 `sin90_outbox` 幂等对账注册到内核 `agent24-scheduler`；`AdjustRhythm` 开放给 UI（仍只走 Proposal 门）。
**验收**：建一条"每周 3 次运动"的 Routine，daemon 重启后 cron 不重复注册（幂等对账的正对照 = 故意注册两次，内核里仍只有一条）。

### M4 —— Review & Markdown

**交付**：Review 三型路由 + `body_ref` 单向 Markdown 外置 + `GET /review/weekly/draft`（自动从事件生成复盘草稿）。
**验收**：周复盘草稿里的"本周 Coding 18h / Business 2h"纯由事件回放得出，不来自任何对话上下文（沿用 SPIKE-00 的判定）。

### M5 —— AI v1

**交付**：三个能力，**只有三个**：`classify`（inbox 条目 → Area/Direction）、`summarize`（事件 → 复盘草稿）、`propose`（排期建议）。全部只产 `Sin90Proposal`，一条都不直写。
**验收**：AI 产出的所有变更在 `sin90_proposals` 里都有 `source ∈ {local_brain, executive}` 的行；拔掉网络后 classify 仍可用（走 oMLX 本地脑）。

### M6 —— Life Packs

**交付**：Learning / Health / Relation / Wealth 四个 Pack 作为**数据**（Area + Direction + Routine 模板的 JSON），`POST /packs/install`。
**验收**：装一个 Pack 不改一行 Rust 代码；卸载一个 Pack 不影响其它 Area 的数据。
**这条是整个设计的试金石**：如果装 Pack 需要改代码，说明 §3 的 Area 抽象没做对。

### M7+ —— 暂不规划

Projection 层（tiny-world-builder 的 `world[x][z]` vs `cellMeshes` 那条思路——**可视化永远是 Projection，不是数据库**，改状态只许走 mutation entry point，在 Sin90 里就是 HTTP 路由 + Proposal 门）。本轮不排期。

---

## 7. 开放问题（2026-09-20 已由用户拍板，记录裁决过程）

### 7.1 要不要在协议层强制"AI 不直写" —— **裁决：Sin90 自己实现，不指望 Agent24**

先评估"这该由谁做"：查过 Agent24 的受约束代理机制（`X-A24-*` 头两个方向全部剥除、内核重新注入不可伪造的 `X-A24-Request-Id`/`X-A24-Approval-Token`），也查过 Agent24 daemon 有没有"这次调用是 AI 发起还是人发起"这个概念——**没有**，全仓搜不到任何 caller-kind/actor-type 之类的东西。这是设计使然：受约束代理是"透明转发"，内核故意不理解模块自己的业务语义。审批 token 之所以能由内核签发，是因为"审批"本身是内核的一等公民概念；"这次写 Sin90 的库是不是 AI 发起的"纯粹是 Sin90 自己的业务语义，Agent24 没有、也不应该有能力回答这个问题。

**结论**：这个约束必须由 Sin90 自己实现和校验，不能依赖 Agent24 提供任何信号。具体做法：签发两类 API key/token（人类会话 key 能直写；自动化/AI key 只能建 Proposal），Sin90 自己签、自己校验，跟审批 token 是两个不同层面的机制，互不依赖。M0 阶段先落成 §7.1 原方案 (a) 的加强版——把"约定"升级成"至少有区分调用者类型的 key"，不是纯自报字段；协议层要不要进一步收紧（原选项 b/c）留给用到真实问题时再说。

### 7.2 M0 要不要等 Agent24 的 T13（`agent24-os-sdk`） —— **裁决：自己写，确认**

维持原判断：自己写 M0 所需的握手代码，返工范围钉死在 `adapter_agent24/` 一层，T13 落地后替换这一层即可。

### 7.3 T11 的依赖顺序 —— **裁决：确认，T11 不依赖 T10**

用户已确认：T11 可以跳过暂停中的 T10（Cos72）直接开工。

### 7.4 五大人生系统要不要随包内置 —— **裁决：可选种子数据，默认内容用用户自己的框架**

不是纯空白启动，也不是硬编码进代码逻辑——做成**安装/初始化时的可选种子数据**：提供"要不要用推荐模板"的选项，选了就用用户自己已经设计好的五大人生系统框架（财富 4 账户/学习 IN→OUT/工作 3+90+复盘/健康睡眠-运动-饮食/关系三层）建出对应的 Area + 初始 Routine，建完之后完全可删可改，不是写死的默认值。这样"这是我的 OS"和"开箱即用"两者都照顾到——种子数据本身就是 §1 描述的用户自己的框架，不是凭空编的示例。

---

## 8. 明确排除的范围（本轮设计不覆盖）

| 不做 | 理由 |
|---|---|
| 2D/3D 人生地图、花园、岛屿 | Projection 层，排在所有数据层之后；现在做等于给一个还会变的模型画皮 |
| 复杂财务模块（对账、账户余额、投资曲线） | 需要外部数据源与金额精度语义，是独立项目而不是 Life OS 的一层 |
| 多用户 / 云同步 / 联邦 | Sin90 刻意单用户 local-first；Agent24 的 Nostr/联邦那几个 crate 不碰 |
| Obsidian 双向同步 | §4.2 已裁决单向 |
| 移动端 / 桌宠壳 | 壳是 Pet0 的职责（见 Agent24 `docs/SIN90-PET0-INTEGRATION.md` §4.2） |
| 非 Rust 参考实现 / wire 文档 | 是 Agent24 的 T13/T14，不是 Sin90 的交付 |
| 内核记忆（ME-3d） | `kernel_capabilities: [events]`，Sin90 自己管数据目录，整条绕开 |
| TELOS 式"理想态文档 + 49 个 AI Skill" | danielmiessler/LifeOS 的那套 AI Harness 复杂度极高且与 Agent24 的能力层重叠；**只借"Current State → Ideal State 是主轴"这个思想**，落成 `Direction.target_window + achieved 终态 + attention 的 planned vs actual`——那就是 Sin90 版的 gap 度量 |

---

## 9. 参考项目：借了什么，没借什么

| 项目 | 借的具体机制 | 在 Sin90 里长成什么 | 没借的 |
|---|---|---|---|
| danielmiessler/LifeOS | Current State → Ideal State 作为主轴；系统的价值是**度量差距**而不是记录清单 | `attention` 的 planned vs actual 就是差距度量；`Direction.target_window` + `achieved` 是 Ideal State 的可判定版本 | TELOS 十二节文档、49 个 Skill、Pulse 守护进程（Agent24 daemon 已是这一层）、Algorithm 七阶段循环 |
| quanru/obsidian-lifeos | Quick Capture（录入摩擦决定存活）；Daily/Weekly/Monthly 周期机制 | M1 的 `POST /capture`；M3 的 `Routine{kind:review}` + 已有的 `Review{daily/weekly/rhythm}` | 把 Obsidian 当运行时；Dataview 查询模型；PARA 的四分法（我们用 Area + Direction 两层，Resources/Archives 用 `status:archived` 表达） |
| luneth90/lifeos | Agent 无关——人生数据是自己的，执行者（Claude Code/Codex/OpenCode）可换；append-only 的 per-item history | `core/` 层零 Agent24 依赖；`sin90_events` 本来就是 per-entity append-only 日志 | 纯 Markdown Vault 作 source of truth（§4.2 已说明为什么付不起）；8 个 MCP memory 工具那套接口（Sin90 走 HTTP，不走 MCP memory） |
| jasonkneen/tiny-world-builder | **单一 mutation entry point**：`setCell(x,z,opts)` 同时改模型、重建网格、刷新受影响邻居；渲染永远是模型的确定性投影 | Sin90 的 mutation entry point 是 HTTP 路由 + Proposal 门，未来任何可视化都只订阅事件流做投影，**绝不反向写库** | 3D/Three.js/任何渲染代码（M7+，本轮排除） |

---

## 10. 变更流程

- 本文是 `iDoris-ai/Sin90` 的设计权威，**只对 Sin90 仓库生效**。
- 涉及 Agent24 内核接口的条款（挂载、进程外协议、事件信封），权威源仍是 Agent24 的 `docs/specs/SPEC-ME3-OUT-OF-PROCESS.md` 与 `protocol/events.schema.json`；**本文引用不复述**——复述的东西必然比来源先过期，这是 Agent24 这个项目反复被咬到的地方。
- 数据模型的任何新增实体，先回本文 §2 的裁决表加一行（含理由与复审触发条件），再动代码。
</content>
