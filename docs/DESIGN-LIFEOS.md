# Sin90 Life OS —— 正式设计文档 v1

> 文档类型：设计裁决（Design Decisions）+ 里程碑拆解
> 输入：[`LIFEOS-DESIGN-INPUT.md`](LIFEOS-DESIGN-INPUT.md)（2026-09-20 用户构想 + GPT 提议）
> 核对对象：Agent24 主仓库 `rust/crates/agent24-sin90{,-store,-os}`（2026-09-20 实读源码，非文档转述）
> 状态：本文冻结数据模型与 M0 范围；M1+ 只给方向，不冻结
> 最后更新：2026-09-24（T5.0.1 §11 AI v1 设计补丁，**设计已冻结 v2.1**（Tier 2 本地评审，记 Codex 债）；§1.3 / §2 #17–#26 / §3.3 / §4.1 / §6 M5 随之更新）

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

`Sin90Op` 内核移植时六个变体：`CreateDirection` / `TransitionTask` / `CreateTasks` / `ReorderTasks` / `AdjustRhythm` / `CarryOverTask`；M0 加 `CreateArea` / `CreateTask`（§3.3），**本仓库今天共八个**（`src/core/proposal.rs:39-87`）。
M5（T5.0.1，§11.2）再加两个：`AssignTaskDirection{task_id, direction_id}`（inbox 任务归到一个 Direction）、`DraftReviewBody{review_id, base_body_sha256, body}`（替换**草稿**复盘正文，带比较并交换）——共十个。
`Sin90Proposal{id,status,source,ops,rationale}`，`source ∈ {local_brain, executive, rule}`。M5 起 `source` 由 AI 模块按**实际服务的引擎**填（§11.3.1：reflex → `rule`，`result.tier = local` → `local_brain`，`remote` → `executive`），不是自报。
`validate` 是**纯函数**（无 I/O），通过 `ValidationCtx` trait 拿 store 快照；内部 `Working` 视图把同批次内前序 op 的效果叠上去，所以 `[t→InProgress, t→Done]` 通过而 `[t→InProgress, t→InProgress]` 失败。M5 给 `Working` 加两张叠加表（任务归属、复盘正文摘要，§11.2.3）。
`Sin90Proposal` 是 `deny_unknown_fields`，**但 `Sin90Op` 枚举本身不是**——`{"op":"create_area","title":"x","bogus":1}` 今天会被静默接受（scratch 测试 `existing_sin90op_silently_accepts_unknown_fields` 实测，§11.1 F-1）；T5.2.1 给 `Sin90Op` 补上 `#[serde(deny_unknown_fields)]`，此前「所有输入结构 `deny_unknown_fields`」这句不成立。

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
| 5 | 加 `Review` 环节 | **已满足（+ T4.1.1 补 `period`）** | `Review{kind: daily/weekly/rhythm, status: draft/finalized}` 内核已有实体与状态机，只缺路由（M4 补）。路由落地时发现旧 `week_id` 列表达不了 daily/rhythm 期间，补 `period TEXT NOT NULL` + `UNIQUE(kind, period)`（§3.2/§4.1，迁移 `0007_review_period.sql`）作为 Review 的实际身份轴；`week_id` 保留但新代码不再写。 |
| 6 | Rhythm 细分 `Routine` | **采纳** | 现有 `Rhythm` 只表达"Direction 间的注意力配额"（`Vec<Alloc>`），表达不了"每周 3 次 ×30 分钟"。`Routine` 是重复发生的执行模板，与 Rhythm 正交，不是它的子类；触发规则是 `cron` + `tz`（IANA 时区名，缺省 `UTC`——cron 本身不含时区，DST 边界必须显式）。M3。 |
| 7 | Rhythm 细分 `ScheduleBlock` | **已满足** | 内核已有，含状态机与三条路由。 |
| 8 | Rhythm 细分 `ReviewCycle` | **拒绝** | 复盘周期 = `Routine{kind: review}` + 已有的 `Review` 实体。第三个对象没有新增表达力。 |
| 9 | Rhythm 细分 `Reminder` | **拒绝** | 提醒是内核 `agent24-scheduler` 的职责。Sin90 只在 `Routine` 上声明触发规则（cron 表达式），经 `sin90_outbox` 幂等对账落到内核。自建 Reminder 实体 = 重写内核已有的调度器。 |
| 10 | Proposal 语义收紧（AI 不直接改状态） | **已满足（但有一处名不副实，见下）** | `sin90_proposals` + CAS 幂等 + 纯函数 validate + 单事务 apply 全都在。语义不改，只随新实体扩 `Sin90Op` 变体。 |
| 11 | Hybrid SQLite/Markdown 存储 | **改造 → T4.2.1 已实现** | 采纳"SQLite 是 operational source of truth"，但收紧 Markdown 的角色：**单向、由行持有指针**（`reviews.body_ref` 指向文件），不做双写、不做双向同步。双向同步是这类系统最大的坑，且没有任何一条 M0-M4 的需求要它。M0 不引入 Markdown。`body_ref` 是**相对 `data_dir` 的路径**（`reviews/<kind>/<period>.md`，见 §4.2），`POST /reviews/{id}/finalize` 时原子写（同目录临时文件 + fsync + rename），先写文件、后提交 DB 事务；`GET /reviews*` 永远读 `body` 列，从不读文件（迁移 `0008_review_body_ref.sql`）。 |
| 12 | 补 Event Log 抽象 | **已满足** | `sin90_events` append-only + 自包含 payload + 水位保证增量==全量重建，且有测试。**硬约束延续**：新增的 Area/Routine 必须同样产事件，重构不得退化成只存快照。 |
| 13 | Domain Logic 不 import Agent24，只有 adapter 知道 | **采纳原则，改造形态** | 见 §4——物理分模块采纳；"standalone 与 Agent24 双轨部署"**拒绝**，它与 T11 验收标准打架。 |
| 14 | M0-M11 里程碑表 | **改造** | GPT 的 M0（"数据模型+SQLite+Event"）内核早就有了，照它排会先把已有能力退化再补回来。重排见 §5。 |
| 15 | outbox 状态扩为 `pending\|done\|failed`（T3.3.1） | **采纳** | `sin90_outbox` 原状态只有 `pending\|done`，表达不了"这条对账意图已经永久失败，不该再重试"。加 `failed` + `failure_kind`/`last_error`/`attempts`/`next_attempt_at` 四列（迁移 `0005_outbox_failed.sql`，纯加列，`status` 本来就没有 CHECK，见 §1.2；编号占 `0005` 而非 spec.md 原分配的 `0006`——T3.2.2 尚未开工，`0005_routine_fires.sql` 还不存在，留空洞会让 T3.2.2 之后落地一个编号更小却更晚出现的迁移，见迁移文件自身注释与 `outbox_migrations_are_contiguous_no_gaps` 测试）。错误分类（spec.md "错误处理"）：**永久**（`forbidden`/`quota_exceeded`/`invalid_params`）→ `failed`，在 `/today` 暴露，Routine 下次变更时重置为 `pending`；**可重试**（`rate_limited`/`busy`/`timeout`/断连/`not_ready`/`draining`）→ 退避后重试，`next_attempt_at` 记录何时可再试。对账落地（T3.3.2）不在本条范围。 |
| 16 | fired 投递去重表 `sin90_routine_fires`（T3.2.2） | **采纳** | 内核调度是**至少一次**投递、同一次到点的重试共用 `fire_id`（Agent24 `docs/design/ME4-S1-scheduler-callback.md` §4.1/§4.2）——Sin90 侧必须按 `fire_id` 幂等去重，否则一次到点的重试会被记成多次 `routine.fired`。表只做去重记录，不做内核那边已有的状态机/重试/退避（那些留在内核的 `schedule_deliveries`，spec.md M3 "fired"）：`fire_id TEXT PRIMARY KEY, routine_id TEXT NOT NULL REFERENCES sin90_routines(id), scheduled_for TEXT NOT NULL, trigger TEXT NOT NULL CHECK(trigger IN ('tick','run_now')), received_at TEXT NOT NULL`（迁移 `0006_routine_fires.sql`，`spec.md` 原文的列集是 `fire_id, routine_id, scheduled_for, received_at`；`trigger` 是本条新加的——投递 body 里本来就带 `trigger`（内核设计文档 §5.3 `FiredBody`），`(routine_id, 来源)` 各自独立去重/审计时用得上，不加就要在 `payload` JSON 里翻找）。`POST /_a24/scheduler/fired` 只在**挂载模式**注册（architecture.md #4：这条路径的可信性来自内核代理剥掉客户端伪造的 `X-A24-*` 头，standalone 模式没有代理这层）；未知 `key`/已 `retired` 的 routine 一律 200 且不写行不发事件，留给对账器（T3.3.2，未做）处理孤儿——这里不反向调内核。 |
| 17 | 新 Op `AssignTaskDirection{task_id, direction_id}`（T5.0.1，M5 classify） | **采纳** | classify 的产出必须是一条可被人批准的变更，现有八个 Op 没有一个能改 `sin90_tasks.direction_id`（人类路由 `PATCH /tasks/{id}` 也只收 `{to}`）。**只在 inbox 上生效**（前置：任务 `direction_id IS NULL` 且非终态；目标 Direction 存在且非终态），所以它同时是一个比较并交换：人先归了类，AI 的旧提议在 accept 时 422，不会覆盖人的决定。事件 `task.direction_assigned`。细则 §11.2.1。**复审触发条件**：出现「改已归类任务的归属」的真实需求（届时加 `from_direction_id` 字段做 CAS，而不是放宽前置条件）。 |
| 18 | 新 Op `DraftReviewBody{review_id, base_body_sha256, body}`（T5.0.1，M5 summarize） | **采纳** | 复盘正文今天只能走人类直写 `PATCH /reviews/{id}`；AI 必须走提议门。`base_body_sha256` 是提议生成时看到的正文摘要——人在提议挂起期间改过正文，accept 即 422（`StaleBase`），人写的字不会被 AI 静默覆盖。只作用于 `draft`，定稿后拒。事件复用 `review.updated`（payload 与人类路径逐字段相同），重放方不需要区分来源。细则 §11.2.2。 |
| 19 | `ValidationCtx` 第二次加宽：`direction_status` / `task_direction` / `review_snap` | **采纳（推翻 §3.3「唯一一次」）** | #17/#18 的前置条件都是「按实体读当前值」，正是这个 trait 的既定职责（`proposal.rs:110-118` 的 SCOPE 注释：per-entity existence + status）。实现者只有 `DbSnapshot` 与测试 Mock，三个方法在 T5.2.1 一次加齐。 |
| 20 | `sin90_ai_calls` 加列 `run_id / proposal_id / served_tier / model_id / prompt_tokens / completion_tokens / error_kind` | **采纳** | 原表（`0001_sin90.sql:120-128`）回答不了验收要的两件事：这条提议是哪次调用产出的（`proposal_id`）、调用实际在本地还是远端服务（`served_tier`——内核按 `complexity` 路由，模块请求的引擎 ≠ 实际服务层级，Agent24 ME4-S2 §2.2/§4.3）。纯 `ADD COLUMN`，全部可空。§11.6。 |
| 21 | 新表 `sin90_settings(key PK, value, updated_at)`，首个键 `ai.executive_enabled` | **采纳** | executive 需要「用户设置开启」，设置必须持久、只由人改、改动留痕。单独一张 KV 表而不是塞进 `actor-keys.json`（那是凭据文件，权限 0600，语义不同）。改动走人类直写路由 + `setting.changed` 事件（#12 的硬约束）。缺行 = 关。 |
| 22 | classify 允许「只指派 Area、不指派 Direction」（给 `sin90_tasks` 加 `area_id`） | **拒绝** | §3.1 的对象图里 Area 在 Direction 之上、不与 Task 并列；`list_tasks` 按 Area 过滤就是经 `sin90_directions` join（`repo.rs:1214-1240`）。加列 = Task 有两条可能互相矛盾的归属路径。classify 在「有 Area 没有合适 Direction」时**不产出提议**（任务留在 inbox，理由写进 run 结果）。**复审触发条件**：用户反馈 inbox 里长期堆着「知道属于哪个领域、但不值得为它开方向」的条目。 |
| 23 | reflex（纯规则）产出的提议 `source = rule` | **改造（改 §M5 验收原文）** | §M5 原文只列 `{local_brain, executive}`，但引擎梯最底层 reflex 不调任何模型；把它记成 `local_brain` 是「措辞比机制强」（§2.1）。`rule` 早在 `ProposalSource` 里（`proposal.rs:89-95`）。T5.5.1 的 SQL 本来就含 `rule`。 |
| 24 | M5 用内核私有记忆（T4.4.1 写入的定稿摘要）做 AI 上下文 | **v1 不用** | SQLite 是真相（architecture.md 边界 #1），summarize 需要的「上周定稿」本地直接可读；走 `_a24/memory/private/recall` 只多一个失败面，且 v1 三个能力没有一个需要跨设备/跨模块上下文。T4.4.1 照做（它的价值是给内核侧 agent 用）。**复审触发条件**：出现需要「非 Sin90 数据」的 AI 能力。 |
| 25 | `CreateTasks` / `CarryOverTask` 的 `task.created` payload 加 `direction_id`（T5.4.1，v2 M2） | **采纳** | 今天只有 `CreateTask` 的 payload 带归属（`repo.rs:1129`、`:2658`），另两处没有（`:2744`、`:2869`），违背 #12「payload 自包含」；按 Direction 回放完成任务数时只能去 join 可变表。只加字段，旧事件缺字段时按 §11.2.1 的规则顺 `carried_from` 回溯。 |
| 26 | Sin90 正式包声明 `model_access: remote_allowed` | **拒绝（硬约束）** | 声明后内核对本模块**所有**调用按 `Privacy::Any` 路由，本地不可用时连 `simple` 调用也会被送到远端，acceptance.md M5「不开远端时只用本地模型」不再成立（§11.3.2）。只在测试包（cargo feature `remote-allowed-manifest`）里声明，用来验证 executive 路径；J10c 钉住正式 manifest。**复审触发条件**：Agent24 提供逐次「只能收窄」的隐私字段，使 `local` 调用在 `remote_allowed` 包里仍由内核保证留在本机。 |
| 27 | `POST /proposals/{id}/reject`，`Proposal.status` 值域实际启用 `rejected`（T5.7.1，Q6） | **采纳** | `ProposalStatus::Rejected`（`core/types.rs:125`）是从内核原样搬来的枚举值——§1.1 表早就写着 `pending / applying / applied / rejected`——但搬进本仓库以来没有任何路由能把提议实际推进到这个态，只是类型层面的死代码。只人类 key（镜像 `accept` 的门禁：自动化能提不能判）；CAS 只认 `pending → rejected`，`applying`/`applied`/已 `rejected` 一律 409（`apply_proposal` 的 CAS-失败分支同一套写法）。去重（`list_pending_proposals` 是 `WHERE status = 'pending'` 的 SQL 级过滤，`repo.rs:2271`）不需要改：状态一变成 `rejected` 就自动不在结果集里，`ai_classify::dedup_targets` / `ai_propose::dedup_propose` 零改动即生效——T5.2.1 的依赖只是"确认"，不是"实现"。**并发互斥**（Opus 2026-09-2x review L4）：`reject_proposal` 与 `apply_proposal` 竞争同一行时的互斥，靠的是两者共用的 `BEGIN IMMEDIATE`（同一 `sin90.db`，SQLite 单写者语义）——不额外写并发测试：`Sin90Store::open_memory` 的写连接池 `max_connections(1)`（`store/mod.rs`），同一进程内对同一内存库的两次"并发"写请求在 sqlx 连接池这一层就已经被串行化，测试根本制造不出真正的竞态窗口，写出来的"并发测试"只是两个先后执行的普通请求，不能证明任何东西。 |
| 28 | 拒绝日志表：独立表 `sin90_proposal_rejections` vs 给 `sin90_proposals` 加列 | **独立表** | 两个方案都能装下要求的字段（提议 id/能力来源/ops 摘要/AI 理由/创建时间/拒绝时间/可选 reason）。选独立表的理由：(a) **只追加、不改原行**——`sin90_proposals` 已经被 `row_to_proposal`/`list_pending_proposals`/`list_ai_produced_proposal_ids`/J8 行级快照等多处按固定列名读取，加列虽是纯 `ADD COLUMN` 不破坏这些查询，但会让"审计日志"语义混进"状态机当前值"语义，两种关注点耦合到同一行里；(b) 用户原话是"为以后学习用户习惯**积累数据**"——这是分析场景，天然是"一个提议对应零或一条历史记录"的日志形状，不是提议本身的属性；(c) J8 的行级快照（`store::test_hooks::snapshot_all_tables`）按表名逐表比对，新增一张表不影响任何现有判据的"排除清单"（`sin90_proposals`/`sin90_ai_calls`/`sin90_events WHERE entity='proposal'`），而在 `sin90_proposals` 上加列会让那些判据的"整行不变"断言也要跟着改。能力来源（`classify`/`summarize`/`propose`/`direct`，Opus review M1 把兜底值从 `manual` 改成 `direct`——自动化 key 也能直接提交，不只是人，`manual` 名不副实）不新增字段跟踪，直接在拒绝时查 `sin90_ai_calls WHERE proposal_id = ? AND ok = 1`（`repo.rs:744` 附近 `list_ai_produced_proposal_ids` 已经用同一 join 形状回答"这条提议是不是 AI 产出、产自哪个能力"）——查不到即直接 `POST /proposals` 提交（op 不绑定 AI 路径，见 `Sin90Op::AssignTaskDirection`/`DraftReviewBody` 的文档）。**`proposal_source` 列**（Opus review M1，同一次迁移补齐）：从 `sin90_proposals.source` 原样复制一份（`local_brain`/`executive`/`rule`）——这是与 `capability_source` **正交的第二根轴**："谁/什么产出了这条提议"（提交时自报）vs "这条提议是不是某次 `/ai/*` 能力运行的直接产物"（拒绝时按 `sin90_ai_calls` 反查）；两者都值得记，互相不能推导出对方（例如同一 `source = local_brain` 的提议，既可能来自一次真实 `/ai/classify` 运行，也可能是人/自动化直接 `POST /proposals` 时自报了这个值）。**前提（Opus review M2，这张表能"只追加"成立的全部基础）**：`sin90_proposals` 的行**永远不删**、`ops` 列**永远不改写**——`submit_proposal` 只有"新建"或"幂等重放同一批 ops"两条路径（它自己的文档：不同 ops 的重复 id 是 `Conflict`，不是覆盖），`accept`/`reject` 只翻 `status`/`decided_at`/`result`，都不碰 `ops`/`created_at`。`ops_summary`/`rationale`/`proposed_at` 正是在这个前提下才能安全地"在拒绝那一刻算一次、写进日志就不用再管"——它们是从一个保证不会再变的 `sin90_proposals` 行派生的。**如果将来要做保留期清理**（删除很旧的 `sin90_proposals` 行），必须先把要删的行的 `ops` 快照进本表（把 `ops_summary` 换成完整 ops 副本，或另加一列），再删源行；顺序反了、或者清理时顺手"顺便"改写了还没被拒绝的提议的 `ops`，都会让本表已经写好的 `ops_summary` 与事实脱节，或让一个"待拒绝"的提议在被拒时算不出摘要。今天没有任何清理代码，这条只是给未来的清理任务立的界。迁移 `0010_proposal_rejections.sql`。 |

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

`Direction`（+ 新增 `area_id` 可空外键）、`Task`（+ 新增 `parent_task_id` 自引用）、`Week`、`ScheduleBlock`、`Rhythm`、`Review`（+ T4.1.1 新增 `period` 必填列，见下）、`Proposal`、`Event`。

字段级改动共三处：
- `sin90_directions.area_id TEXT REFERENCES sin90_areas(id)` —— 可空，未分类的 Direction 仍合法（无损迁移）。
- `sin90_tasks.parent_task_id TEXT REFERENCES sin90_tasks(id)` —— 可空；**约束：不允许自引用成环，且只允许一层**（M0 只查"父任务自己不能有父任务"，两行 SQL，比通用环检测便宜且够用；需要多层时再放开）（无损迁移）。
- `sin90_reviews.period TEXT NOT NULL`（T4.1.1，§4.1）—— 必填新列，靠迁移时 `DEFAULT ''` + 回填满足 SQLite 的 `ADD COLUMN NOT NULL` 限制；`UNIQUE(kind, period)` 让 (kind, period) 成为 Review 的实际身份轴，取代原先只能表达"这一周"的 `week_id`（`week_id` 列保留，新代码不再写）。

### 3.3 `Sin90Op` 的扩展

新增变体（M0）：
```
CreateArea   { title }
CreateTask   { title, direction_id?, parent_task_id?, kind?, energy?, est_minutes? }
```
`CreateTasks{week_id, tasks}` 保留（它是"往某周批量塞任务"，与上面单条创建不是一回事）。

`ValidationCtx` 相应扩两个方法：`area_exists(&self, id) -> bool`、`task_parent(&self, id) -> Option<Option<TaskId>>`（外层 `None` = 任务不存在，内层 `None` = 没有父任务）——这是本次唯一一次加宽这个 trait，加宽是破坏性变更，所以一次加够。

> **M5 更正（T5.0.1）**：「唯一一次」在 M5 被推翻——AI v1 的两个新 Op 需要 `direction_status` / `task_direction` / `review_snap` 三个读法（§11.2.3、§2 #19）。实现者只有 `store::repo::DbSnapshot` 与两处测试 Mock（都在本 crate 内），破坏面可控；三个方法**在 T5.2.1 一次加齐**（含 T5.3.1 才用的 `review_snap`），不分两次加宽。

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
| `sin90_reviews`（T4.1.1 + T4.2.1） | + `period TEXT NOT NULL`（`daily=YYYY-MM-DD` / `weekly=YYYY-Www` / `rhythm=<rhythm_id>`），+ `UNIQUE(kind, period)`（`sin90_reviews_kind_period_uq`）；`week_id` 列保留（旧列不删）但新代码不再写它——`period` 取代它成为 Review 的唯一身份轴。迁移 `0007_review_period.sql`：本表在此之前从未被任何代码写入过（无 `INSERT INTO sin90_reviews`），所以回填在实际存量库上不可达；仍防御性实现——有 `week_id` 且指向真实 Week 的行回填该周 `iso_week`，其余回填 `'legacy-' || id`（`id` 是主键，不会与唯一约束冲突）。+ `body_ref TEXT NULL`（T4.2.1，迁移 `0008_review_body_ref.sql`，纯 `ALTER TABLE ... ADD COLUMN`，既有行读回 `NULL`）——定稿（`draft → finalized`）时写入，值是**相对 `data_dir` 的路径** `reviews/<kind>/<period>.md`（见 §4.2）；草稿始终 `NULL`。 | 加列 |
| `sin90_ai_calls`（T5.1.1，§11.6） | + `run_id TEXT NULL`、`proposal_id TEXT NULL`、`served_tier TEXT NULL`（`local\|remote`，reflex/失败为 `NULL`）、`model_id TEXT NULL`、`prompt_tokens INTEGER NULL`、`completion_tokens INTEGER NULL`、`error_kind TEXT NULL`；+ `idx_sin90_ai_calls_run(run_id)`、`idx_sin90_ai_calls_proposal(proposal_id)`。`task_kind` 的值域定为 `classify\|summarize\|propose`，`engine` 仍是 `reflex\|local\|executive`（请求的引擎）。迁移取当时的 max+1（spec.md「迁移编号不预分配」），与下一行同一个文件 | 加列 |
| `sin90_settings`（T5.1.1，§11.6） | `key TEXT PRIMARY KEY, value TEXT NOT NULL`（JSON 标量）`, updated_at TEXT NOT NULL`；v1 唯一的键 `ai.executive_enabled`，缺行 = `false` | **新** |
| 其余 7 张 | 不变 | 旧 |

**不做的事**：不重建表、不改现有列类型、不动 `sin90_attention_*` 的水位语义。用户的 `sin90.db` 靠迁移升级，不靠重建（Agent24 §4.3 既有硬约束）。

### 4.2 Markdown（M4 才引入）

| 用途 | 落法 |
|---|---|
| 复盘正文（T4.2.1 已实现） | `<data_dir>/reviews/<kind>/<period>.md`（`data_dir` = `sin90.db` 所在目录，standalone `--data-dir` 与挂载模式的 `A24_DATA_DIR` 都一样），`sin90_reviews.body_ref` 存**相对 `data_dir` 的路径**（即 `reviews/<kind>/<period>.md`），单向指向，只在 `finalize` 时写一次；旧的 `~/.agent24/os/sin90/notes/<yyyy>/<id>.md` 路径是本节早期草稿，未落地，以此表为准。 |
| 年度总结、人生原则、研究笔记、AI Report | 尚未设计（M4 之后），路径与落法留待各自的 task 定稿时再补 |
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
**T5.0.1 设计补丁（§11，v2.1 已冻结）对这段的两处收紧/改动**：
1. 「Area/Direction」落成**只指派 Direction**（Area 由 `direction.area_id` 派生）——Task 没有 `area_id` 列，inbox 的定义是 `direction_id IS NULL`，只给 Area 不会让任务离开 inbox（§2 #22）。
2. 引擎梯最底层是**无模型的 reflex 规则**，它产出的提议 `source = rule`（而不是冒充 `local_brain`，§2 #23）。所以验收句改为：AI 模块产出的每条提议都有 `source ∈ {local_brain, executive, rule}`，且与产出它的那次 `sin90_ai_calls` 记录（引擎 + 实际服务层级）一致；「断网 classify 仍可用」= 远端不可达、本地可达时 classify 产出 `source = local_brain` 的提议（判据 J16/J23）。

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

---

## 11. M5 设计补丁 —— AI v1：新 Op、引擎梯、三能力契约（T5.0.1）

> **设计已冻结（v2.1，2026-09-24）**。第 2 轮 REQUEST_CHANGES 只余 1 个 High（H1 残留），统筹裁定「修完即冻结、不再整轮送审、由统筹复核」；v2.1 折入该 High 与第 2 轮全部 M/L（见「v2 → v2.1 改动记录」）。
> **全部轮次都是 Tier 2 本地评审**（全新上下文 Opus 子代理；Codex 额度 2026-09-29 19:28 前耗尽），记 **Codex 评审债**：额度恢复后请 Codex 补审，重点 ① §11.4.2 叙述复核的绕过面（Unicode 规范化、渲染端差异）；② §11.5 syn 白名单的绕过面（宏、`$tt` 拆路径、字符串形式路径）；③ §11.4 公共「SAVEPOINT 试跑」依赖的「`apply_op` 无非数据库副作用」不变式。
>
> | 轮次 | 评审方 | 结论 | C / H / M / L |
> |---|---|---|---|
> | 第 1 轮（v1，`2ffe621`） | 全新上下文 Opus 子代理（Tier 2，Codex 额度耗尽；验证 crate `scratchpad/review-probe/`，依赖 `t501-check`） | REQUEST_CHANGES（两个新 Op 的校验、`Working` 叠加、accept 时 CAS 成立） | 0 / 3 / 8 / 7 |
> | 第 2 轮（v2，`6dd7f84`） | 全新上下文 Opus 子代理（Tier 2；验证 crate `scratchpad/review2-probe/`、`scratchpad/selfsuper/`）；H2、H3 CLOSED | REQUEST_CHANGES（余 H1 残留，修完即冻结） | 0 / 1 / 5 / 7 |
| v2.1 | 统筹复核（不整轮送审） | —— | —— |
>
> 事实核对基线：本 worktree `docs/t5.0.1-ai-v1-design`（叠在 `feat/t4.2.1-body-ref` `7925c4d` 上）；类型化客户端在分支 `feat/t3.2.1-kernel-clients`（`db80b88`）；内核推理回调以 Agent24 `docs/design/ME4-S2-model-callback.md` **v3.1 冻结版**为准（本文引用不复述，§10）。
> 文中每一段 Rust 签名都在 scratch crate `t501-check`（path 依赖本 worktree 的 `sin90`）里 `cargo check --all-targets` + `cargo test` + `cargo clippy --all-targets` 过，见 §11.12。
> 标 ⚖️ 的数值是**选的**，不是推出来的。标 🟡 的是**待用户拍板的产品问题**（§11.11），文中给的是拍板前的**最保守占位**，不是结论。

#### v2 → v2.1 改动记录（第 2 轮：REQUEST_CHANGES，0 C / 1 H / 5 M / 7 L，全部采纳）

| 条 | 问题（评审原意，标「实测」的在 review2-probe 里跑过） | v2.1 改法 | 位置 |
|---|---|---|---|
| H1 残留 | 模型能手写 `〔标签：数值〕` 单元（`〔领域「Coding」投入：十八 小时〕`、`〔…：翻倍〕，〔完成任务数：全部〕`）；数词与量词间隔空格/零宽（`十八 小时`、`十八​小时`）、占位与量词间隔空格/零宽（`{{f3}} 小时`）、`本​周数字`/`本　周数字`、单独 `\r`（CommonMark 按行切、Rust `lines()` 不切）、任务标题可含 `\r` 与 Markdown 经 `{{tN}}` 注入——均实测通过 | ① 字面段出现 `〔〕「」` 直接拒（只有程序能产生它们）；② 检查前先拒控制字符、删 Unicode Cf 格式字符、把非换行空白压成一个空格，数词/占位与量词之间允许「可选一个空格」，「本周数字」在去掉全部空白后比较；③ 除 `\n` 外全部控制字符与 U+2028/2029 一律拒，按 `\n` 切行（不用 `lines()`）；代入的任务标题与数字块里的名称走 `sanitize_inline`（删控制与 Cf、压平空白与换行、转义行内 Markdown、`< > &` 换全角、`〔〕「」` 换成别的括号）；另补：`{{tN}}`/`{{fN}}` 的 N 只认无符号无前导零的 ASCII 数字（`{{t+1}}` 不再被解析）、大写数词（壹…萬）入表、叙述里 `<` `&` 换全角（HTML 块/实体不成形）；④ J17 补全部对应负对照，scratch `round2_bypasses_are_rejected` 25 条 | §11.4.2、J17 |
| H2 文字 | 「`self`/`Self` 放行」让 `use self::super::super::store::Sin90Store;` 过检查 | 先剥掉开头的 `self` 再按 `super` 个数判断；scratch 加 `use` 与表达式两种写法的正对照 | §11.5、J7 |
| H3 Low | 试跑依赖的不变式没写 | 写明不变式：**`apply_op` 永远只做数据库写，不做任何非数据库副作用**（文件、内核调用、事件外发都不在 `apply_op` 里；`finalize_review` 的 Markdown 写不是 Op），并由 J21b 的结构断言钉住 | §11.4 公共、J21b |
| M1 | `read_only(true)` 在 `open_memory` 下不成立（另开池连到另一个空库；共享缓存配 `read_only(true)` 仍能写） | 读取器改为：与写连接**同一目标**的连接选项 + `pragma("query_only","ON")`；测试模式下 `open_memory` 改为**按实例命名的共享缓存内存库**（`file:sin90-<ULID>?mode=memory&cache=shared`），读取器池连同一个名字。scratch `readonly.rs` 实测：文件库（WAL）与共享缓存两种模式下读取器都读得到、写即 `readonly` 错；共享缓存下读取器遇到另一任务未提交的写事务会**等待**到其提交（不报错），同一任务持写事务时读会自锁——ai run 从不在持有 sink 事务时读（sink 方法是原子调用） | §11.5、J8 |
| M2 | `is_program_only` 误判（数字块下人手写一行仍判为程序生成） | 改为与 `render_facts(当前草稿)` **逐字比较**（或正文为空）；旧数字块（数字已变）也算人写——保守 | §11.4.2、Q7 |
| M3 | 汉字数词规则误杀「这一周」「一个」「一次」 | 提示词写明这些写法会被拒、改用不带数词的说法；J26 统计该规则引起的降级比例；scratch `known_false_positives_are_what_m3_says` 把误杀写成断言 | §11.4.2、J26 |
| M4 | J7 与 ai 模块内 `#[cfg(test)] mod tests` 冲突 | 写死：**恰好** `#[cfg(test)]` 的条目跳过检查（单元测试可以用真实 store 建夹具）；`cfg(any(test, …))` 等其它 cfg 照常检查；scratch 有跳过与不跳过两条断言 | §11.5、J7 |
| M5 | `std` 白名单过宽 | `std`/`core`/`alloc` 下拒 `fs`、`process`、`net`、`os` | §11.5、J7 |
| L | `$a:tt` 拆路径、本地同名 `fn sqlx`；字符串形式路径（`deserialize_with = "crate::store::…"`）；行内链接 `[x](javascript:…)`；`{{t+1}}`；`still_valid_pending` 前后矛盾与 AiSink 代码块漏 `precheck`；每条预检都争写锁 | 前三条写进残余风险 R2/R14（链接由渲染端处理，写明渲染端契约）；`{{t+1}}` 修正解析；删掉 `AiReadModel` 注释里的 `still_valid_pending`，`AiSink` 代码块补 `precheck`；`precheck` 改为**批量**：每 run 一次 `BEGIN IMMEDIATE`，每条草稿一个 SAVEPOINT，最后整体 ROLLBACK | §11.5、§11.9 |

#### v1 → v2 改动记录（第 1 轮：REQUEST_CHANGES，0 C / 3 H / 8 M / 7 L，全部采纳）

| 条 | 问题（评审原意） | v2 改法 | 位置 |
|---|---|---|---|
| H1 | `{{fN}}` 只代数值、标签由模型写：「编码投入达到{{f2}}」（f2 实为 Business）、「完成任务{{f3}}小时」、在叙述区仿造「## 本周数字（修正）」都能通过 | 取消只代数值的占位：唯一的数值占位 `{{fN}}` 由程序**整体**渲染成 `〔标签：数值〕`，数值永远带着它自己的标签；占位后紧跟 `MEASURE` 量词即拒；叙述里出现「本周数字」即拒；叙述逐行剥掉行首 Markdown 块语法（井号、减号、星号、加号、大于号、竖线、反引号、波浪号、等号、下划线），只剩纯段落；J17 加标签错位、仿造数字块两个负对照 | §11.4.2、J17 |
| H2 | J7 黑名单被合并 import 绕过（`use crate::{core::Sin90Op, http::Sin90State};` + `s.store.update_review_body(..)`）；按 `//` 切注释会吃掉 `"http://x"` 之后的代码 | 检查器改用 `syn` 解析：展开全部 use 树，访问所有表达式/类型/模式路径、宏路径与宏 token 里的 `a::b` 链；**白名单**——以 `crate`/`sin90`（或足够多 `super` 到达 crate 根）开头的路径下一段只能是 `core`/`ai`，外部 crate 只能是列表内的；注释与字符串由 syn 处理；正对照加合并 import、`Sin90State` 两种写法等 18 条 | §11.5、J7、§11.12 |
| H3 | T5.4.1「非法建议被 validate 拒」做不到：`ReorderTasks` 的 validate 不查任务存在/在本周（`proposal.rs:262-271`），J21 两可 | `AiSink::submit` 在同一个 `BEGIN IMMEDIATE` 里：`allowed_ops` → `validate` → `SAVEPOINT` + 逐个 `apply_op` 试跑 → `ROLLBACK TO SAVEPOINT` → 插 `pending`；J21 改为确定断言「submit 拒绝且不写任何行」；scratch `dryrun.rs` 在 sqlx 0.8 + SQLite 上实测嵌套事务即 SAVEPOINT、回滚不留痕 | §11.4 公共、J21 |
| M1 | 后台 run 的 Busy/RateLimited 不该按「交互式等不起」降到弱规则；缺调用预算与总时限；Draining/Revoked 应中止 | 动作三分：**降级** / **延后**（`Busy`/`RateLimited`/`NotReady`：本 run 停掉所有模型步，当前与剩余条目记 `deferred`，**不走 R2**）/ **中止**（`ConnectionLost`/`Cancelled`/`Draining`/`Revoked`/`NotSent`）；每 run 模型调用预算 20 ⚖️（≤ 桶容量 30，两级梯每条目最多 2 次）、总时限 600s ⚖️，用完即延后；另加 run 内熔断：`no_provider`/`backend_config`/`forbidden` 之后该引擎本 run 不再调用 | §11.3.4、§11.3.5 |
| M2 | `CreateTasks`/`CarryOverTask` 的 `task.created` payload 不含 `direction_id`（`repo.rs:2744`、`:2869`） | T5.4.1 给这两处 payload 只加字段 `direction_id`（`CarryOverTask` 取源任务的）；§11.2.1 与给 T4.3.1 的注记补「归属 = 最近一次 `direction_assigned`，否则 `task.created.direction_id`，旧事件缺字段时顺 `carried_from` 链回溯」 | §11.2.1、§2 #25 |
| M3 | 调用记录与提议应原子写；`source` 不该由 ai 传入；scratch 写入顺序错 | `AiSink::submit(cap, ProposalDraft, AiCallRecord)`：`ProposalDraft` 没有 `source`/`status` 字段，store 在同一事务里按 `(engine, served_tier)` 推导 `source`、插提议、插 `ok=1` 的调用行；scratch `run_item` 不再自己写产出行，把它随 `Outcome::Produced` 交给调用方 | §11.3.5、§11.5 |
| M4 | J23 包 B 空转（`MODEL_ACCESS` 是 `include_str!` 编译期常量） | 包 B 用 cargo feature `remote-allowed-manifest` 编译（`include_str!` 另一份 `domain-os.remote-allowed.yml`，同一份文件随包 B 安装）；J23 断言调用表里有 `engine='executive'` 行；加钉子 `sin90 print-model-access` 输出 == 已安装 manifest 解析值 | J23、J23b |
| M5 | 汉字数词规则措辞过强；任务标题无法引用 | 措辞收窄为「不含**`MEASURE` 表里的**量词紧跟汉字数词串」；加 `{{tN}}`（渲染 `「任务标题」`，标题里的数字来自数据而非模型） | §11.4.2 |
| M6 | Q7「改写非空草稿」实为整体替换 | 拍板前占位改为：只在正文为空、或正文只有程序生成的数字块时才提议（`is_program_only`） | §11.4.2、Q7 |
| M7 | Q1 应为冻结硬约束 | 正式包**不声明** `remote_allowed`（acceptance「不开远端时只用本地模型」排除了 (b)）；manifest 钉子测试钉住；executive 只在测试包 B 可达；绊线表述保持「检测不是防护」。Agent24 侧「逐次只能收窄的隐私字段」followup 由统筹登记 | §11.3.2、§2 #26、J10c |
| M8 | J8 排除清单不确定 | `AiReadModel` 的 store 实现走**独立只读连接池**（`SqliteConnectOptions::read_only(true)`），写即报错；J8 排除清单写死为三项（`sin90_proposals`、`sin90_ai_calls`、`sin90_events WHERE entity='proposal'`），`sin90_attention_*` **不**排除——它是读时可被 `attention_apply_new_events`（`attention.rs:83`）折叠的派生投影，AI 读路径若去折叠它，J8 应当变红 | §11.5、J8 |
| L1 | AI 提交缺 `proposal.submitted` 内核镜像事件 | http 组装层在 `submit` 成功后补发 `emit("proposal.submitted", {id})`，与 `POST /proposals`（`http/mod.rs:623-626`）同形 | §11.4 公共 |
| L2 | J17 判据改为「每个数字串出现在 `render_facts` 输出里」 | 采纳，并把 `{{tN}}` 渲染的标题算进允许集；带篡改正对照 | J17 |
| L3 | 去重「仍然有效」要可判定 | = 现在对该挂起提议重跑提交前校验（validate + 试跑）能通过；目标 Direction 已 abandoned 的挂起提议不再挡任务 | §11.4 公共 |
| L4 | 采用远端回复前应重读设置 | 采纳：`tier == remote` 时先重读 `AiSettings` 再判绊线（run 途中用户关掉开关也生效） | §11.3.2 |
| L5 | R1 无结论不是失败 | 记 `error_kind='undecided'`，降级统计里排除 | §11.3.5 |
| L6 | 父子任务归属 | 写明：父子任务可以归不同 Direction，本 Op 不检查一致性（与 `CreateTask` 一致） | §11.2.1 |
| L7 | `task_ids` 超 20 的行为；`result.usage` 未记录 | `task_ids` 超 20 → 400（不截断）；`sin90_ai_calls` 加 `prompt_tokens`/`completion_tokens` | §11.4.1、§11.6 |
| Q | Q1/Q2/Q5 不是开放产品问题 | Q1 → 硬约束（不声明）；Q2 → 默认关；Q5 → `rule`（tasks.md T5.5.1 已含 `rule`；DESIGN §M5 已同步，tasks.md T5.2.1 文字在规划分支，由统筹同步）；Q3/Q4/Q6/Q7（新占位）/Q8 仍留给用户，Q8 附评审的技术输入 | §11.11 |

### 11.0 解决什么、不解决什么

**解决**（tasks.md T5.0.1 目标 + T5.1.1–T5.5.1 的验收都要在这里有落点）：

| 问题 | 位置 | 一句话结论 |
|---|---|---|
| classify 的产出落成什么变更 | §11.2.1、§2 #17/#22 | 新 Op `AssignTaskDirection{task_id, direction_id}`，只作用于 inbox；Area 由 Direction 派生，不单独指派 |
| summarize 的产出落成什么变更 | §11.2.2、§2 #18 | 新 Op `DraftReviewBody{review_id, base_body_sha256, body}`，只作用于草稿，正文摘要做比较并交换 |
| 校验放哪、与 `Working` 叠加怎么交互 | §11.2.3、§2 #19 | `ValidationCtx` 加三个读法；`Working` 加两张叠加表；apply 里补关系约束 |
| 引擎梯 | §11.3 | classify：reflex(决定性) → [executive] → local → reflex(兜底)；summarize/propose：[executive] → local → reflex。失败按种类**降级 / 延后 / 中止**；每 run 有调用预算与总时限 |
| executive 的开关在哪、默认什么 | §11.3.2、§2 #21/#26 | 需要 manifest `remote_allowed` **且** `sin90_settings['ai.executive_enabled'] = true`；**正式包不声明 `remote_allowed`（硬约束）**，开关默认关 |
| 三个能力各自的输入/输出/规则/提示词/触发/限流 | §11.4 | 三条 `POST /ai/*` 触发路由，后台 run、单飞、批量与调用预算；模型输出一律 `response_format: json_schema, strict` + 程序复核 |
| 非法建议在提交时就被拒 | §11.4 公共 | 提交前校验 = `validate` + 在 SAVEPOINT 里试跑 `apply_op` 后回滚 |
| summarize「数字只来自草稿」怎么保证 | §11.4.2 | 数字块由程序渲染；叙述里数值只能以程序渲染的 `〔标签：数值〕` 原子单元出现；叙述只剩纯段落 |
| AI 模块碰不到直写接口 | §11.5 | `ai/` 只依赖三个 trait；syn 白名单结构测试 + 只读连接池 + 行为级表快照 |
| 判据 | §11.7 | J1–J26（含 J10b/J10c/J23b），覆盖 T5.1.1–T5.5.1，每条带正对照 |

**不解决**（写进 §11.9 残余风险或留给后续）：
- 提议的**拒绝**路由（今天没有 `POST /proposals/{id}/reject`，过期提议只能一直 `pending`）——🟡 Q6。
- 改**已归类**任务的归属、跨 Area 迁移 Direction、AI 建 Direction/Area（§2 #17/#22 的复审触发条件）。
- daily / rhythm 复盘的 summarize（T4.3.1 只给周草稿；没有数字来源就没有「数字只来自草稿」可言）。
- 按日 token/费用预算（内核 ME4-S2 §0 明确不做；Sin90 只有次数、预算与并发上限）。
- 流式输出、tools / function calling（内核不开）。
- HTTP `POST /proposals` 的提交期校验（F-2，§11.9 R7）——AI 路径自己做提交前校验，人类/自动化 key 的 HTTP 路径 v1 行为不变。

### 11.1 现状（本 worktree `7925c4d` 与 `feat/t3.2.1-kernel-clients` `db80b88` 逐条实读）

1. **提议门**：`Sin90Op` 八个变体（`src/core/proposal.rs:39-87`）；`ProposalSource{LocalBrain, Executive, Rule}`（`:89-95`）；`Sin90Proposal` 带 `deny_unknown_fields`（`:97-108`）。
   `ValidationCtx` 五个方法、只暴露「按实体的存在性 + 状态」（`:110-134`，SCOPE 注释写明关系约束归 store）。`Working` 只叠加任务状态一张表（`:165-186`）。`validate` 首错即返（`:193-202`）。
2. **F-1**：`Sin90Op` 枚举**没有** `deny_unknown_fields`——scratch `ops::tests::existing_sin90op_silently_accepts_unknown_fields` 实测 `{"op":"create_area","title":"x","bogus":1}` 反序列化成功。
3. **提交不校验**：`Sin90Store::submit_proposal`（`src/store/repo.rs:2067-2111`）只做 `INSERT … ON CONFLICT(id) DO NOTHING` + 同 id 不同 ops 报 `Conflict`，**不跑 `validate`**；`validate` 只在 `apply_proposal` 里、`BEGIN IMMEDIATE` 之下跑（`:2168`），失败整事务回滚、提议回到 `pending`（F-2）。
4. **apply**：`build_snapshot`（`:2519`）按 op 载入所需实体；`apply_op`（`:2594`）每个 op 一个分支、每次变更同事务 `append_event`；任务类变更先过关系约束 `require_task_week_open`（`:2894`）。`ReorderTasks` 引用不在该周的任务时在 apply 里 `affected != 1 → NotFound`（`:2753-2770`），整事务回滚——「非法建议被拒而不是半应用」今天就成立。
5. **HTTP**：`POST /proposals` 用 `require_any_actor`（`src/http/mod.rs:614`），`POST /proposals/{id}/accept` 用 `require_human`（`:646`，Codex 2026-09-22 High 的修复）；`StoreError::Proposal` → 422（`:82-86`）。`PATCH /tasks/{id}` 只收 `{to}`（`:143-144`）——**人类今天也没有「给任务改归属」的直写路由**。
6. **inbox**：`today_view` 的 inbox = `direction_id IS NULL AND status NOT IN ('done','dropped')`（`repo.rs:1334-1341`）。Task 没有 `area_id` 列；`GET /tasks?area_id=` 经 `sin90_directions` join（`:1214-1240`）。
7. **复盘正文**：`update_review_body`（`repo.rs:2328`）——`finalized` 先拒（409），与当前正文相同则无写无事件，否则 `UPDATE` + 事件 `review.updated` payload `{review_id, body}`。`finalize_review`（`:2428`）不看有没有挂起的提议。
8. **调用记录表**：`sin90_ai_calls(id, task_kind, engine, fallback_from, latency_ms, ok, at)`（`src/store/migrations/0001_sin90.sql:120-128`）——没有 run、没有提议关联、没有实际服务层级、没有错误种类。
9. **manifest**：`requires_models: []`（`domain-os.yml:14`）、`kernel_capabilities: [events]`（`:20`），没有 `model_access`（= 内核缺省 `local_only`）。
10. **类型化客户端**（`feat/t3.2.1-kernel-clients`）：`ClientError` 闭集（`src/adapter_agent24/clients/error.rs:59`）；`map_rpc_error`（`:287-306`）未知 `data.kind` 一律 `Other`——**内核新增的 `unavailable`（ME4-S2 §7）和既有的 `cancelled` 今天都会落到 `Other`**，`data.retryable` / `data.cause` 读不到。还没有 model 客户端。
11. **本端超时**：`transport.rs:94` `RESPONSE_TIMEOUT = 35s`（为内核 30s 通用超时留余量），而内核给 `_a24/model/complete` 的是 **120s**（ME4-S2 §5.2）——不改就会被本端提前判超时、把「内核还在算」误报成 `timeout`。`call_with_timeout`（`:402`）已有，按调用覆盖。
12. **内核推理回调（ME4-S2 v3.1，冻结）要点**：params `{messages 1..=64, response_format?: json_schema, max_tokens? 1..=4096 缺省 1024, complexity? simple|complex, request_id?}`（§4.2）；result `{text, model_id|null, tier: local|remote, usage}`（§4.3）；隐私**只**由 manifest `model_access` 决定，`local_only` 下内核只路由到 Local/Lora，`remote_allowed` 下以 `Privacy::Any` 路由、`simple` 本地优先、`complex` 远端优先、**选哪个 provider 全由内核**（§2.2）；每模块在途 2、令牌桶 30 突发 / 0.5 每秒（§5.2）；满了不排队直接 `busy` / `rate_limited`；错误新增 `unavailable{retryable, cause ∈ no_provider|request_rejected|backend_config|response_too_large}`（§7）；被代理请求的总时限 30s（ME4-S2 §1 第 9 条 `UPSTREAM_DEADLINE`）。
13. **T4.3.1 周草稿**：本 worktree 未实现（分支 `feat/t4.3.1-weekly-draft` 尚无提交）；形状以 spec.md M4 为准：`{week, by_area:[{area_id,minutes}], by_direction:[…], tasks_done, routines:[{routine_id, fired, completed}]}`。
14. **（v2，M2）`task.created` payload 不一致**：`CreateTask`（直写与提议两条路径）的 payload 含 `direction_id`（`repo.rs:1129`、`:2658`），`CreateTasks` 的只有 `{id, week_id, title}`（`:2744`），`CarryOverTask` 新任务的只有 `{id, week_id, carried_from}`（`:2869`）——重放方今天无法只凭事件知道这两类任务的归属。
15. **（v2，H3）`ReorderTasks` 的 validate 只查周开放、列表非空、无重复**（`proposal.rs:262-271`），不查任务存在、不查任务属于该周；这些在 apply 里才查（`repo.rs:2753-2770`）。所以「非法建议被 validate 拒」对 `ReorderTasks` 不成立，必须把 apply 本身拿来试跑（§11.4 公共）。同理 `idx_sin90_task_carried` 唯一索引（一个任务只能被顺延一次，§1.2）也只在 apply 时才会响。
16. **（v2，M8）attention 物化视图**：`attention()` 是纯读回放（`attention.rs:55`），但 `attention_apply_new_events()`（`:83`）会写 `sin90_attention_daily` 与水位行——它是一个「读侧」可能顺手调用的写者。

### 11.2 新 Op（§2 #17–#19、#25）

两个 Op 都加进 `Sin90Op`（wire：`#[serde(tag = "op", rename_all = "snake_case")]`，与既有一致），同时给整个枚举补 `#[serde(deny_unknown_fields)]`（F-1）。scratch `ops.rs` 用一个平行枚举验证了 wire 形状与「内部标签枚举 + `deny_unknown_fields`」的组合确实拒未知键。

```rust
// scratch src/ops.rs（已 check + test）；实现时是 Sin90Op 的两个新变体
pub const MAX_REVIEW_BODY_BYTES: usize = 64 * 1024;          // ⚖️

#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum NewOps {
    AssignTaskDirection { task_id: TaskId, direction_id: DirectionId },
    DraftReviewBody { review_id: ReviewId, base_body_sha256: String, body: String },
}
/// 小写 hex 的 SHA-256；比较并交换的令牌
pub fn body_sha256(body: &str) -> String;
```

#### 11.2.1 `AssignTaskDirection{task_id, direction_id}`

**为什么是新 Op 而不是复用**：既有八个 Op 里能碰任务的是 `CreateTask`/`CreateTasks`（造新行）、`TransitionTask`（只改状态）、`ReorderTasks`（只改 `sort_key`）、`CarryOverTask`（关旧开新）——没有一个改 `direction_id`。用「`CreateTask` 带 direction + `TransitionTask` 把旧的 drop 掉」拼出来会丢掉任务 id、事件链断成两条、`created_at` 重置（`/today` 的 oldest-first 排序被打乱），且 inbox 条目被 drop 在复盘里会被当成「放弃」统计。所以新增，且只做一件事。

**为什么没有 `area_id`**：§2 #22。classify 的结论「属于 X 领域」体现为「选 X 领域下的某个 Direction」；提议的 `rationale` 里写出领域名（给人看），数据上 Area 永远由 `direction.area_id` 派生。

**validate（纯函数，按批内顺序，经 `Working`）**，全部满足才过：

| # | 规则 | 失败 → `ProposalError` 新变体 | HTTP（accept 时） |
|---|---|---|---|
| A1 | 任务存在（`Working.task_direction` 或 `ctx.task_direction` 外层 `Some`） | `UnknownEntity{entity:"task"}`（既有） | 422 |
| A2 | 任务当前状态（叠加后）**非终态**（`task_is_terminal` 为假：不是 done/dropped/carried_over） | `TaskClosed{task_id, status}` | 422 |
| A3 | 任务当前归属（叠加后）为 `None`，即仍在 inbox | `NotInInbox{task_id, direction_id}` | 422 |
| A4 | Direction 存在（`ctx.direction_status` 为 `Some`） | `UnknownEntity{entity:"direction"}` | 422 |
| A5 | Direction **非终态**（draft/active/paused 可；achieved/abandoned 拒） | `DirectionClosed{direction_id, status}` | 422 |

通过后 `Working.task_direction[task_id] = Some(direction_id)`。
**同批次交互**：`[Assign(t,d1), Assign(t,d2)]` → 第二条 A3 失败；`[TransitionTask(t→dropped), Assign(t,d)]` → A2 失败（`Working` 的任务状态表本来就被 `TransitionTask`/`CarryOverTask` 写）；`[Assign(t,d), TransitionTask(t→planned)]` 合法。**不支持批内前向引用**：同批 `CreateDirection` 产生的 id 在 apply 时才铸造，提议里无法指到它（v1 classify 不建 Direction，§2 #22）。

**apply（`apply_op` 新分支，`BEGIN IMMEDIATE` 之下）**：
1. `require_task_week_open(tx, task_id)`（关系约束，与其它任务变更一致：在 reviewing/closed 周里的任务是历史，不许动）。
2. 读 `sin90_directions.area_id`（事件自包含用）。
3. `UPDATE sin90_tasks SET direction_id = ?, updated_at = ? WHERE id = ? AND direction_id IS NULL`，`rows_affected != 1` → `Conflict`（validate 之外的第二道 CAS，防 snapshot 与 apply 之间任何遗漏）。
4. 事件：`entity = task`、`kind = direction_assigned`（对外 `task.direction_assigned`）、`from_state = to_state = NULL`（状态没变）、payload **自包含**：`{"task_id", "from_direction_id": null, "direction_id", "area_id": <apply 时 direction 的 area_id 或 null>}`。
5. `build_snapshot` 新增：`AssignTaskDirection` 载入任务（状态 + 归属）与 Direction 状态。

**对重放方的影响（交给 T4.3.1）**：任务的 Direction 归属不再只在 `task.created` 的 payload 里。按 Direction/Area 统计「完成任务数」的回放，一个任务在其完成事件时刻的归属 =
1. 该时刻之前最后一条 `task.direction_assigned` 的 `direction_id`；没有则
2. `task.created` payload 的 `direction_id`（v2 起 `CreateTasks` 与 `CarryOverTask` 也写这个字段，§2 #25，T5.4.1 落地）；字段缺失（旧事件）则
3. 若 payload 有 `carried_from`，对源任务递归同样的规则（顺延链最长 = 周数，`idx_sin90_task_carried` 保证无分叉）；仍无则归「未分类」。
attention 回放按 `ScheduleBlock.direction_id` 计时，不受影响。

**父子任务**（v2，L6）：父任务与子任务可以归不同的 Direction；本 Op 不检查父子归属一致性（与既有 `CreateTask` 不检查一致）。一个 Project 横跨两个方向是合法的建模。

#### 11.2.2 `DraftReviewBody{review_id, base_body_sha256, body}`

**为什么不复用 `update_review_body`**：它是人类直写路径；AI 只能提议。**为什么要 `base_body_sha256`**：提议挂起期间人很可能自己在改草稿，accept 一条基于旧正文的 AI 提议 = 静默覆盖人写的字。带上生成时看到的正文摘要，apply 时对不上就拒——这是「AI 不替我做主」在复盘正文上的具体形态。

| # | 规则 | 失败 → `ProposalError` 新变体 | HTTP |
|---|---|---|---|
| D1 | `body.trim()` 非空 | `BlankField{field:"body"}`（既有） | 422 |
| D2 | `body.len() ≤ MAX_REVIEW_BODY_BYTES`（64 KiB ⚖️，UTF-8 字节） | `TooLarge{field:"body", max_bytes}` | 422 |
| D3 | `base_body_sha256` 是 64 个小写 hex 字符 | `BadHash` | 422 |
| D4 | review 存在 | `UnknownEntity{entity:"review"}` | 422 |
| D5 | review 状态 `draft` | `ReviewNotDraft{review_id}` | 422 |
| D6 | 当前正文摘要（叠加后）== `base_body_sha256` | `StaleBase{entity:"review", id}` | 422 |
| D7 | `sha256(body) != 当前摘要`（不是空操作） | `NoChange{op:"draft_review_body"}` | 422 |

通过后 `Working.review_hash[review_id] = sha256(body)`——同批两条作用于同一复盘，第二条必须以第一条的新摘要为 base（scratch `draft_body_cas_chain_and_rejections` 钉住链式与失败两个方向）。
D2 只约束 AI 路径；人类 `PATCH /reviews/{id}` 今天没有正文上限（§11.9 R8）。

**apply**：`build_snapshot` 载入 `status` 与 `body`（在 Rust 里算摘要，不在 SQL 里）；`apply_op` 分支：`UPDATE sin90_reviews SET body = ?, updated_at = ? WHERE id = ? AND status = 'draft'`（`affected != 1` → `Conflict`）；事件 `entity = review`、`kind = updated`、payload `{"review_id", "body"}`——**与人类路径逐字段相同**，一条正文变更一种事件，重放方不需要知道它来自提议（来源经 `sin90_proposals.result.event_ids` 可追）。

#### 11.2.3 `ValidationCtx` 加宽与 `Working`

```rust
// scratch src/ctx.rs（已 check + test）。实现时三个方法直接加进 ValidationCtx（不是 supertrait）
pub struct ReviewSnap { pub status: ReviewStatus, pub body_sha256: String }
pub trait ValidationCtxV5: ValidationCtx {
    fn direction_status(&self, id: &str) -> Option<DirectionStatus>;
    /// 外层 None = 任务不存在；内层 None = 在 inbox
    fn task_direction(&self, id: &str) -> Option<Option<DirectionId>>;
    fn review_snap(&self, id: &str) -> Option<ReviewSnap>;
}
// Working 新增：task_direction: HashMap<TaskId, Option<DirectionId>>、review_hash: HashMap<ReviewId, String>
```

`ProposalError` 新增八个变体：`NotInInbox`、`TaskClosed`、`DirectionClosed`、`ReviewNotDraft`、`StaleBase`、`NoChange`、`TooLarge`、`BadHash`（scratch `NewOpError` 是它们的原型）。全部经既有 `StoreError::Proposal` → 422，不新增 HTTP 映射。

### 11.3 引擎梯契约（T5.1.1）

#### 11.3.1 三级引擎

| 引擎 | 是什么 | `ProposalSource`（store 推导，§11.3.5） | 可用条件 |
|---|---|---|---|
| `reflex` | 纯规则，进程内，只读 Sin90 自己的库 | `rule` | 永远可用（含 standalone、内核未授予 models、断网） |
| `local` | `_a24/model/complete`，`complexity: simple` | 按 `result.tier`：`local` → `local_brain` | 握手 `Offer.provides` 含 `_a24/model/`（`ModelClient::new` 返回 `Some`） |
| `executive` | `_a24/model/complete`，`complexity: complex` | 按 `result.tier`：`remote` → `executive`，`local` → `local_brain` | 上一行 **且** 编译期 `MODEL_ACCESS == remote_allowed` **且** `sin90_settings['ai.executive_enabled'] == true`。**正式包里前一条恒假**（§11.3.2） |

**关键事实**：Sin90 **选不了** provider（ME4-S2 §2.2）。「executive」只是「请内核按 `complex` 路由」，内核可能仍用本地服务它；所以 `source` 按 **`result.tier`（实际服务层级）** 定，不按请求的引擎（scratch `executive_request_served_locally_is_local_brain`）。

#### 11.3.2 executive 的两道闸，与正式包的硬约束

1. **manifest `model_access`**（内核强制）：Sin90 用 `include_str!` 在编译期取随包 manifest 的值（`const MODEL_ACCESS`），只用来决定**要不要尝试** executive；真正的隐私保证在内核。
2. **用户设置**（Sin90 自己的）：`sin90_settings` 的 `ai.executive_enabled`，缺行 = `false`；只能经 `PUT /settings/ai {"executive_enabled": bool}` 改（`require_human`，`deny_unknown_fields`，同事务写事件 `setting.changed` payload `{key, value}`），`GET /settings/ai` 读（任一 key）。

**为什么正式包不声明 `remote_allowed`（硬约束，§2 #26）**：一旦声明，内核对 Sin90 的**所有**调用都以 `Privacy::Any` 路由——包括 `local` 引擎发的 `simple` 调用：本地 provider 不可用时内核会**自己**把它送到远端（ME4-S2 §2.2「`Simple` 本地优先」= 本地不行就远端），模块没有逐次收窄隐私的字段（ME4-S2 §4.2）。acceptance.md M5「不开远端时只用本地模型」因此只在 `local_only` 下成立。所以：
- **正式包**：`domain-os.yml` 不写 `model_access`（= `local_only`），`local` 调用**由内核保证**不出本机（以 ME4-S2 §2.3 的 `Local` 定义为准），executive 永不尝试。钉子测试 J10c 钉住。
- **测试包 B**（cargo feature `remote-allowed-manifest`，只用于 J16/J23 验证 executive 路径）：`include_str!("../domain-os.remote-allowed.yml")`，同一份文件随包安装。它的已知性质：开关关时 Sin90 不发 `complex` 调用，但内核仍可能把 `simple` 调用送到远端；Sin90 只能**事后发现**——`result.tier == remote` 时**先重读 `AiSettings`**（run 途中用户关掉开关也生效，L4），开关关则丢弃结果、记 `error_kind='privacy_tripwire'`、降级（scratch `switch_turned_off_mid_run_trips_remote_reply`）。这是**检测，不是防护**：字节已经出去了。
- 让正式包也能安全地用 executive，需要 Agent24 给 `_a24/model/complete` 一个**只能收窄**的逐次隐私字段；该 followup 由统筹在 Agent24 侧登记，落地前本约束不放松。

manifest 同时要改：`kernel_capabilities` 加 `models`（M3/M4 另加的 `scheduler`/`memory` 不归本文）；`requires_models` **保持 `[]`**——它是挂载时资源检查（ME4-S2 §1 第 11 条），写了会让「没有本地模型」的机器装不上 Sin90，而 reflex 让 AI 在没模型时仍可用。

#### 11.3.3 每个能力的梯（`plan()`，scratch `ports.rs` 已 check + test）

```rust
pub enum Step { ReflexDecisive, Model(Engine), ReflexFallback }
pub fn plan(cap: Capability, access: ModelAccess, settings: AiSettings, model_port_present: bool) -> Vec<Step>;
// classify : [ReflexDecisive, Model(Executive)?, Model(Local)?, ReflexFallback]
// summarize: [Model(Executive)?, Model(Local)?, ReflexFallback]
// propose  : [Model(Executive)?, Model(Local)?, ReflexFallback]
// Executive 出现 ⇔ port 在 ∧ access == RemoteAllowed ∧ settings.executive_enabled；Local 出现 ⇔ port 在
```

- **向上（升级）**：只有 classify 有 `ReflexDecisive`——规则能**确定**时不花模型（§11.4.1 R1）；确定不了记 `undecided`，不是失败，`fallback_from` 不记，降级统计里排除（L5）。
- **向下（降级）**：模型尝试失败按 §11.3.4 处理；降到下一步时，下一步那行的 `fallback_from` = 刚失败的引擎。
- **同一引擎不原地重试**（v1）。

#### 11.3.4 失败 → 动作（`ModelFailure::action()`，scratch `failure_table_is_exhaustive` 用穷尽表钉住）

`ai/` 不认识 `ClientError`（它不许 import `adapter_agent24`，§11.5）；adapter 实现 `ModelPort` 时把 `ClientError` 折叠成 `ModelFailure`。三种动作：

- **降级**：记本步失败，走梯的下一步（最终可到 R2 / 纯数字块 / reflex 排期）。
- **延后**：容量用尽——本 run **停掉所有模型步**；当前条目与剩余条目结束为 `deferred`，**不走 reflex 兜底**（弱规则不该因为内核忙而替代模型；用户稍后再触发一次即可）。
- **中止**：本 run 立即结束，不再产出任何提议；已提交的保留。

| `ClientError`（T5.1.1 补齐后） | `ModelFailure` | 动作 | 熔断 | `error_kind` |
|---|---|---|---|---|
| `Unavailable{retryable:true, cause:no_provider}` | `Unavailable` | 降级 | **是** | `unavailable.no_provider` |
| `Unavailable{retryable:false, cause:request_rejected}` | `Unavailable` | 降级 | 否（下一条目的请求不同） | `unavailable.request_rejected` |
| `Unavailable{retryable:false, cause:backend_config}` | `Unavailable` | 降级 + `warn!` | **是** | `unavailable.backend_config` |
| `Unavailable{retryable:false, cause:response_too_large}` | `Unavailable` | 降级 + `error!`（本端 `max_tokens` ≤ 1024，出现即 bug） | 否 | `unavailable.response_too_large` |
| `Timeout`（本端 125s 或内核 120s） | `Timeout` | 降级 | 否 | `timeout` |
| `Forbidden`（未授予 models；正常构造时 port 为 `None` 走不到） | `Forbidden` | 降级 + `warn!` | **是** | `forbidden` |
| `InvalidParams` / `PayloadTooLarge` | `BadRequest` | 降级 + `error!`（本端 bug） | 否 | `bad_request` |
| `NotFound` / `QuotaExceeded` / `TokenInvalid` / `RequestNotInFlight` / `Other` | `Other` | 降级 | 否 | `other` |
| `Busy` / `RateLimited` | `Busy` / `RateLimited` | **延后** | —— | `busy` / `rate_limited` |
| `NotReady`（握手未完成） | `NotReady` | **延后** | —— | `not_ready` |
| `Draining` / `Revoked` / `NotSent` | `GenerationEnding` | **中止**（这一代正在结束，architecture.md 边界 #5） | —— | `generation_ending` |
| `ConnectionLost` | `ConnectionLost` | **中止**（结果不确定） | —— | `connection_lost` |
| `Cancelled`（内核停机） | `Cancelled` | **中止** | —— | `cancelled` |
| 模型返回了但输出不合格（§11.4 各能力的复核） | —— | 降级 | 否 | `bad_output` 等 |
| 返回了，`tier == remote` 而（重读后的）开关关 | —— | 降级 | 否 | `privacy_tripwire` |

**熔断**（run 内）：标「是」的失败之后，该引擎在本 run 剩余条目里不再调用（直接当作已失败，下一步 `fallback_from` = 它），省下预算、不反复撞一个已知不可用的后端（scratch `circuit_breaker_skips_engine_for_rest_of_run`）。

**每 run 预算**（`RunState`，⚖️）：模型调用 ≤ `MAX_MODEL_CALLS_PER_RUN = 20`（内核令牌桶突发 30 之内；两级梯每条目最多 2 次，所以一个 run 覆盖的条目可能少于 20）；总时限 `RUN_DEADLINE_SECS = 600`。预算或时限用尽 → 同「延后」（scratch `call_budget_defers`）。

T5.1.1 对 `ClientError` 的配套改动（在 adapter 里）：加 `Unavailable{retryable: bool, cause: UnavailableCause}` 与 `Cancelled` 两个变体；`map_rpc_error` 认 `unavailable`（读 `data.retryable`、`data.cause`，cause 不在四值闭集内 → `Other`）与 `cancelled`；`every_spec_error_kind_maps_to_its_documented_variant` 的计数 17 → 18（`unavailable`）并把 `cancelled` 从「落 `Other`」移出；`is_permanent`：`Unavailable{retryable:false}` 为真；`is_retryable`：`Unavailable{retryable:true}` 为真。

#### 11.3.5 `sin90_ai_calls`：每次尝试一行，产出行与提议同事务

**每个 `Step` 实际执行一次写一行**（熔断跳过、延后未执行的步不写）。scratch `AiCallRecord`：

| 列 | 值 |
|---|---|
| `id` | ULID |
| `run_id`（新） | 本次 `POST /ai/*` 的 run id；classify 的一个 run 覆盖多个条目 |
| `task_kind` | `classify` / `summarize` / `propose` |
| `engine` | **请求的**引擎 `reflex` / `local` / `executive` |
| `fallback_from` | 上一步失败（或被熔断）的引擎；升级（R1 `undecided` → 模型）不记 |
| `served_tier`（新） | 模型返回时的 `result.tier`；reflex 与传输失败为 `NULL` |
| `model_id`（新） | `result.model_id` |
| `prompt_tokens` / `completion_tokens`（新，L7） | `result.usage` |
| `latency_ms` | 本步墙钟（含内核排队与推理） |
| `ok` | 本步**产出了可用结果**（模型返回且复核通过 / reflex 有结论） |
| `error_kind`（新） | §11.3.4 的串；R1 无结论 `undecided`；R2 无结论 `no_match`；产出但提交前校验失败 `rejected_by_precheck`；`ok = 1` 时为 `NULL` |
| `proposal_id`（新） | 只在产出提议的那一行填，由 store 在 `submit` 事务里填 |
| `at` | ISO-8601 |

**原子性（M3）**：`run_item` 对产出的那一步**不写**调用行，把 `ok=1` 的 `AiCallRecord` 随 `Outcome::Produced` 返回；调用方把它和 `ProposalDraft` 一起交给 `AiSink::submit(cap, draft, rec)`，store 在**同一事务**里插提议、插调用行（`proposal_id = draft.id`）。所以「每条 AI 提议恰好对应一行 `ok=1` 调用记录」是事务保证，不是尽力而为。`submit` 被拒（`SinkError::Invalid`）→ 调用方改写这行为 `ok=0, error_kind='rejected_by_precheck'` 经 `record_call` 写入，继续下一条目（不降级——模型没错，是状态变了）。
**`source` 由 store 推导**：`ProposalDraft` 没有 `source`、`status` 字段；store 用 `source_for(rec.engine, rec.served_tier)`（reflex → `rule`；`served_tier = remote` → `executive`；否则 `local_brain`）。ai 模块没有任何途径写入一个与调用记录不一致的 `source`。
非产出步的 `record_call` 失败只 `warn!`，不阻断 run（R6）。
**propose 的产出步是这条「一条产出对应一行」原则的例外（T5.4.1 实现时补，2026-09-24 review L-d）**：propose 一次决策可能同时拆成 carry/reorder/create 至多三条独立提议，一步最多写**三行**调用记录，具体记账规则见 §11.4.3 自己的小节。

### 11.4 三个能力的契约

**公共部分**：
- **触发**：三条路由，全部 `require_any_actor`（触发本身只写 `sin90_proposals` 的 `pending` 行与 `sin90_ai_calls`，不改业务状态），返回 `202 {"run_id", "capability"}`，run 在后台跑：
  `POST /ai/classify {"task_ids"?: [...]}`、`POST /ai/summarize {"review_id"}`、`POST /ai/propose {"week_id"}`；`GET /ai/runs/{run_id}` 返回 `{run_id, capability, state: running|done|aborted|unknown, items: [{target, result: proposed|nothing|deferred|rejected|skipped|aborted, reason?: human_text|dedup}], calls: [...]}`（运行态在进程内存，上限 64 条 LRU ⚖️；重启后只剩 `calls`，`state = unknown`）。*T5.2.1 实现时补*：`aborted` = 该条目因 run 中止（绊线/总时限/panic）未处理，与去重导致的 `skipped` 区分；淘汰只淘汰非 `running` 的 run（`running` 的由 `BusyGuard` 的 `Drop` 保证最终 `finish`，不另设超时）。*T5.3.1 实现时补（2026-09-26 review M2）*：`reason` 只在 `result = "skipped"` 时出现，区分「去重挡住」（`dedup`）与「summarize 自己的 Q7 人写文字门」（`human_text`）；其它 `result` 一律不带这个字段（序列化时整个省略，不是 `null`）。
  standalone 模式同样注册（port 为 `None`，只有 reflex）。**不在** `/_a24/*` 下。
- **为什么后台跑、不绑 `request_id`**：被代理请求的总时限是 30s（§11.1 第 12 条），一次本地推理可以到 120s；绑上就会被截断（`RequestNotInFlight`）。所以模型调用**不带 `request_id`**，run 属于 Sin90 进程；进程退出时在途 run 丢弃（已提交的提议与已写的调用记录保留）。
- **限流（Sin90 这一侧）**：每个能力**单飞**（再触发 → `409 {"code":"ai_busy","run_id"}`）；进程内模型调用信号量 = 2（= 内核每模块在途上限）；`_a24/model/complete` 用 `call_with_timeout(125s)` ⚖️；每 run 调用预算 20、总时限 600s（§11.3.4）。
- **自动触发**：🟡 占位 = **v1 只有手动触发**。见 Q3。
- **提议形状**：`id = "ai-<capability>-<ULID>"`（便于人看；「是不是 AI 产出」以 `sin90_ai_calls.proposal_id` 关联为准，J24）；`rationale` = `"<engine>: <理由>"`，去控制字符、截到 280 字符 ⚖️；`source` 由 store 推导（§11.3.5）。
- **提交前校验（H3）**：`AiSink::submit` 的 store 实现在**一个** `BEGIN IMMEDIATE` 里依次：
  1. `allowed_ops(cap)`——classify ⇒ 仅 `AssignTaskDirection`；summarize ⇒ 仅 `DraftReviewBody`；propose ⇒ 仅 `CarryOverTask`/`ReorderTasks`/`CreateTasks`（J22）；
  2. `build_snapshot` → `validate`；
  3. **试跑**：开嵌套事务（sqlx 在已开事务的连接上 `begin()` 即发 `SAVEPOINT`），对每个 op 调**同一个** `apply_op`，然后**无条件** `ROLLBACK TO SAVEPOINT`——关系约束（`ReorderTasks` 引用的任务是否在该周、`require_task_week_open`、`idx_sin90_task_carried` 唯一索引、CAS UPDATE 的 `affected`）在这里全部真实地跑一遍，试跑里铸造的 ULID 与事件随回滚消失；
  4. 推导 `source`，`INSERT` 提议（`pending`）+ `proposal.submitted` 事件行 + `ok=1` 调用行；`COMMIT`。

  **试跑依赖的不变式（v2.1）**：`apply_op` 永远只做 `sin90.db` 内的读写，**不做任何非数据库副作用**——不写文件、不调内核、不外发事件、不改进程内状态。今天成立（`repo.rs:2594` 起的每个分支都只有 SQL 与 `append_event`）；定稿写 Markdown 在 `finalize_review` 里，不是 Op；内核副作用走 outbox（apply 只写 outbox 行，随回滚一起消失）。将来任何 Op 若需要非数据库副作用，必须放到 apply 之后（outbox 或 http 层），不许进 `apply_op`。J21b 钉住。
  任一步失败 → `SinkError::Invalid`，整个事务回滚，**不写任何行**。scratch `dryrun.rs` 在 sqlx 0.8 + SQLite 上实测了「嵌套事务 = SAVEPOINT、回滚后外层照常插入、试跑不留痕、非法重排整体拒绝」。accept 时照旧 validate + apply（状态可能已变）。人类/自动化 key 的 `POST /proposals` 不变（F-2）。
  **镜像事件（L1）**：`submit` 成功后，http 组装层（持有 `EventSink` 的那一层，不是 `ai/`）补发 `emit("proposal.submitted", {"id"})`，与 `POST /proposals` 同形（`http/mod.rs:623-626`）。
- **去重（L3）**：触发时跳过已有**仍然有效**挂起提议的目标。「仍然有效」= 现在对那条挂起提议重跑提交前校验的第 1–3 步能通过。由 `AiSink::precheck(cap, &[ProposalDraft]) -> Vec<bool>` **批量**完成：每个 run 开头调用一次，一个 `BEGIN IMMEDIATE`、每条草稿一个 SAVEPOINT（试跑后 ROLLBACK TO），最后 ROLLBACK 整个事务——一次 run 只争一次写锁，不改变任何行。于是：任务已被归类、正文已被人改、目标 Direction 已 abandoned 的挂起提议都不再挡新 run——否则没有拒绝路由（Q6）时，一条过期提议会永久挡住它的目标。**各能力可以在本节「仍然有效」的基础上收紧，见各自小节**（T5.4.1 实现时补，2026-09-24 review L-c）：
  - **classify**：去重不区分挂起提议的来源——人类直接提交的一条同目标 `AssignTaskDirection` 提议，只要仍然有效，**照样挡住** AI 的再分类。这是**有意的**：人对同一个任务已经有一条挂起的分类判断时，不该让 AI 再提一条可能冲突的（两条都指向同一个 inbox 任务，接受一条就会让另一条在 accept 时因为 `NotInInbox` 而失败——避免这个竞争，比"只挡 AI 自己产出的"更保守也更安全，且分类这个能力的挂起提议本来就该只有一条）。
  - **propose**：T5.4.1 走的是相反的口径——去重**只认 AI 自己产出的提议**（§11.4.3 自己的小节详述），人类/automation 直接提交的同形状提议不挡 AI。两个能力选了不同的口径，都是有意的：分类的挂起提议本来就该唯一，人类提交的一条已经"占住"了这个任务，AI 没有必要（也不应该）再抢着提一条冲突的；而 propose 一次周期里人和 AI 都可能各自调整 carry/reorder/create，人类的手工调整不该被当成"AI 已经处理过"从而拦住 AI 继续给建议。
  - **summarize**（T5.3.1 实现时补，2026-09-26 review M4）：与 classify 相同的口径——去重不区分来源，任何仍然有效的挂起 `DraftReviewBody` 提议都挡住新的一次触发。理由与 classify 不同但结论一致：`DraftReviewBody` 是整篇正文的 CAS 整体替换（§11.2.2），同一个 `base_body_sha256` 上若同时存在两条挂起提议，先被 accept 的那条会把正文摘要往前推进一格，另一条在自己被 accept 时必然因为 `StaleBase` 而失败——两条提议本就不可能同时生效，允许它们并存只是制造必然浪费的挂起行，不是给用户多一个选择。

#### 11.4.1 classify（T5.2.1）

- **输入**：`task_ids` 给了就用——至多 20 个 ⚖️，**超过 → 400**（不截断，L7），每个必须在 inbox，否则 400；没给就取 inbox 里最老的、未被去重挡掉的至多 20 条。调用预算（20）可能先于条目耗尽，剩下的记 `deferred`。
- **候选集**：非终态 Direction，按 `updated_at` 倒序至多 **40** 个 ⚖️（`{direction_id, title, status, area_title}`）；候选为空 → 该条目结束为 `nothing`（不写调用行、不产提议）。
- **reflex R1（决定性）**：`normalize_title`（去首尾空白、压缩内部空白、小写，scratch 已测）后，库里**已归类**、归属 Direction 仍非终态的任务中，同标准化标题的任务**全部**指向同一个 Direction D → 提议 `AssignTaskDirection(t, D)`，`source = rule`，不调模型；否则 `undecided`。
- **模型（executive / local）**：
  - 候选以**不透明短键** `d1…dn` 呈现（scratch `candidate_keys`），模型看不到、也就造不出 ULID。
  - messages：`system` = 固定指令（「从候选里选一个最合适的方向；都不合适就选 none；只输出 JSON」）；`user` = JSON `{"item": {"title": …}, "candidates": [{"key":"d1","title":…,"area":…}, …]}`。
  - `response_format`：`{"type":"json_schema","json_schema":{"name":"sin90_classify","strict":true,"schema": <scratch classify::schema>}}`——`{choice: enum[d1…dn, none], confidence: enum[low, medium, high], reason: string ≤ 200}`，`additionalProperties: false`。`max_tokens: 256` ⚖️。
  - 程序复核（scratch `classify::parse`）：容忍一层 ```` ```json ```` 围栏；`deny_unknown_fields`；`choice` 必须在本次键集内（否则 `bad_output`）；`none` 或 `low` → 本条 `nothing`（记 `ok = 1`——模型认真地说了「不知道」）🟡 Q8。
- **reflex R2（兜底，仅在模型步全部失败/熔断或不存在时；延后不走）**：任务标题与「候选 Direction 标题 + Area 标题」的重合度——ASCII 按长度 ≥ 3 的词、CJK 按字二元组；**唯一最高分**且 ≥ 2 个二元组或 ≥ 1 个词 ⚖️ → 提议；否则 `no_match`。R2 是弱规则，产出的提议 `source = rule`，人一眼能看出不是模型给的。
- **输出**：每个条目至多一条提议，`ops = [AssignTaskDirection]`，条目之间独立。

#### 11.4.2 summarize（T5.3.1）

- **输入**：`review_id`，必须 `kind = weekly` 且 `status = draft`（否则 409；daily/rhythm 400 `unsupported_kind`）；周 = `period`。
- **可改写条件（v2.2，2026-09-26 review C1 定案，取代 v2.1 的 Q7 占位）**：当前正文去尾部空白后为空，或逐字等于下列程序渲染之一（去尾部空白后比较）：① `render_facts(当前草稿)`；② `render_weekly_draft_markdown(当前草稿)`（T4.3.2 自动建的周草稿正文，由 `AiReadModel::weekly_draft` 在同一次读取中一并返回，`ai/` 不自行渲染，见 `ports::SummarizeDraft::auto_draft_md`）。不做模糊匹配：在任一程序渲染上多、少或改一个字符即算人写，run 结果记 `skipped: human_text`——`DraftReviewBody` 是整体替换，人写过的字不应出现在「整体改写」提议里。**T4.3.2 必须在同一事务内、写入自身 `routine.fired` 事件之后渲染草稿**（`store::weekly_draft::weekly_draft_on`/`record_routine_fire` 的实现约束），否则它自己的触发计数会让 ② 永远不与之后任何一次重新读取相等——这正是 C1（critical）发现的 bug：v2.1 只认①、且 T4.3.2 的草稿正文一直是②，两者从未相等过，summarize 因此永远无法接手一份自动创建的草稿。保守的代价（仍然接受）：数字块下多出人手写的一行、或旧周数字块（数字已变，即①②都对不上）都会被当成人写，AI 不再覆盖它。**②的时效性（2026-09-26 review L3，仍然接受）**：`auto_draft_md` 只在自动建草稿那一刻是「当下」的——草稿生成之后，同一周里只要再发生任何会改变 `weekly_draft` 数字的事件（新完成一个任务、新记一次专注块、routine 再 fire 一次……），一次新的 `weekly_draft` 读取就会算出和当初存下来的②不一样的文本，②也就跟着失配，落回「人写」判定，AI 不再覆盖。这与①的「旧周数字块」是同一类代价，只是②发生得更早、更容易撞到——一次自动建稿到 summarize 真正触发之间只要有一件事发生就可能失配。缓解不是代码层面的，是排程层面的：review 节律的 cron 应该尽量安排在**贴近周界**（例如周日或周一凌晨，紧邻 `iso_week_bounds` 的结束/开始），让"自动建稿"与"这一周基本定形"之间的窗口尽量短，降低②在被 summarize 用上之前就先失配的概率。
- **数字来源**：`AiReadModel` 调 T4.3.1 的周草稿函数得到 `WeeklyDraft`（形状见 §11.1 第 13 条）；另取该周完成任务的标题（至多 50 条 ⚖️），只作为可引用的素材。
- **「数字只来自草稿」的机制**（scratch `summarize.rs` 已 check + test）：
  1. `facts(draft, title_of)` 把草稿每个数值变成 `Fact{key: "fN", label, value}`，标签里的领域/方向/节律名先过 `sanitize_inline`；`render_facts` 渲染「本周数字」块——**完全由程序生成**。
  2. 模型只输出 `{"narrative": string ≤ 2000}`（`response_format` 同上，`max_tokens: 1024` ⚖️）。提示词给它 `[{key, label, value}]` 与 `[{key: "tN", title}]`，并写明：不写任何数字（含汉字数词）；要引用数值只能写 `{{fN}}`，引用任务只能写 `{{tN}}`；不要自己写 `〔〕「」`；不要用标题、列表、表格、引用、HTML；**「这一周」「一个」「一次」这类「汉字数词 + 量词」的日常写法也会被拒，请换成不带数词的说法（「本周」「某个」「再次」）**（v2.1 M3——规则会误杀它们，J26 统计由此引起的降级比例）。
  3. `fill_narrative` 按固定顺序处理（v2.1 H1）：
     1. **拒控制字符**：除 `\n` 外任何控制字符（含单独的 `\r`、制表符）与 U+2028/U+2029 → 拒。原因：CommonMark 等渲染端把单独 `\r` 当换行，Rust `lines()` 不当——两边切行不一致就是绕过口。
     2. **规范化**：删除 Unicode Cf 类格式字符（零宽空格、零宽连接符、方向控制、BOM 等，按 Unicode 15 的码位表），把每一段非换行空白（含全角空格）压成一个 ASCII 空格。之后所有检查都在规范化文本上做。
     3. **拒**（`bad_output`，降级）：
        - 占位之外的字面文本里有任何 `char::is_numeric()` 为真的字符（阿拉伯、全角、罗马数字 `Ⅻ` 等；`&frac12;` 这类实体因含数字也被拒）；
        - 字面文本里出现 `〔` `〕` `「` `」`——这四个括号只能由程序渲染产生，模型手写即拒（封住「手写单元」）；
        - 汉字数词串（`〇零一二两三…百千万亿`、大写 `壹…萬`）之后、隔**至多一个空格**紧跟 `MEASURE` 表里的量词（小时、分钟、个、件、次、项、天、周、%、倍、成）；
        - `{{fN}}` 之后隔至多一个空格紧跟 `MEASURE` 量词（给数值单元换单位）；
        - 占位键不是 `f`/`t` + 无符号、无前导零的 ASCII 数字（`{{t+1}}`、`{{t01}}`、`{{ f1 }}` 都拒），或编号不存在；未闭合；
        - 去掉全部空白后含「本周数字」（仿造数字块）。
     4. **渲染**：`{{fN}}` **整体**渲染为 `〔标签：数值〕`，数值永远带着自己的标签；`{{tN}}` 渲染为 `「sanitize_inline(标题)」`——任务标题是用户数据，可能含 `\r`、Markdown、HTML，`sanitize_inline` 删控制与 Cf、把包括换行在内的空白压成一个空格、给行内 Markdown 元字符加反斜杠、`< > &` 换全角、`〔〕「」` 换成 `［］『』`（标题里伪造的单元不再像单元）。
     5. **压平**：按 `\n` 切行，每行剥掉行首块语法（井号、减号、星号、加号、大于号、竖线、反引号、波浪号、等号、下划线），`<` `&` 换全角（HTML 块与实体不成形），丢空行，段落间空一行——叙述只能是纯段落。
  4. 正文 = `compose_body(数字块, Some(叙述))`；reflex（兜底）= `compose_body(数字块, None)`。
  结论的**准确说法**：正文里每个数字串都出现在程序渲染的数字块里，或出现在程序代入的任务标题里（J17 的判法）；叙述里出现的每个 `〔…〕` 单元都由程序渲染、标签与数值来自同一条 `Fact`；叙述不含字面数字，不含「汉字数词 +（至多一个空格）+ `MEASURE` 表里的量词」，不含除 `\n` 外的控制字符与格式字符，不含块级 Markdown/HTML。**不**声称：模型写在单元前后的文字与单元一致（「编码投入达到〔领域「Business」投入：2 小时 0 分钟〕」能通过——矛盾可见但未被阻止，R1）；不声称模型无法表达数量（「近半」「翻倍」「seven」、不在表里的量词仍能漏过，R1）；行内链接 `[x](url)` 不在这里处理，由渲染端负责（R14）。
- **比较并交换**：`base_body_sha256` = 触发时读到的正文摘要；新旧正文相同 → 不产提议（run 结果 `nothing`）。
- **数字的时效**：数字冻结在提议生成时；人重新触发即可（R4）。

#### 11.4.3 propose（T5.4.1）

- **输入**：`week_id` = 目标周 W，必须 open（planning/active），否则 409。
- **读**：W 的非终态任务；**上一周** P = `iso_week` 小于 W 的最近一周且仍 open（reviewing/closed 周里的任务不许动）——P 不存在或已关就没有顺延建议；Rhythm 当前配额（非 retired 的最新一条）；W 里没有任何任务、但配额 pct > 0 的 Direction = 「缺口 Direction」。
- **只用三个既有 Op**，分成**至多三条独立提议**（人可以分开批）：
  1. `propose.carry`：`[CarryOverTask(t, W) …]`，t ∈ P 中 planned/in_progress 的任务（`backlog → carried_over` 不合法，`transitions.rs:121-133`）。
  2. `propose.reorder`：`[ReorderTasks{week_id: W, order}]`，`order` 是 W 全部非终态任务的一个**排列**（程序保证：模型漏掉的按原顺序补在后面，重复即 `bad_output`）；与当前 `sort_key` 顺序相同 → 不产。
  3. `propose.create`：`[CreateTasks{week_id: W, tasks: [{title, direction_id}]}]`，至多 3 条 ⚖️，`direction_id` 只能是缺口 Direction；**只有模型步产出**（reflex 不编标题）。
- **reflex**：carry = P 中全部 planned/in_progress；reorder = in_progress → planned → backlog，同层按所属 Direction 的配额 pct 降序、再按 `created_at`；create = 无。
- **模型**：候选任务与缺口 Direction 用不透明键（`p1…` / `w1…` / `g1…`）；schema `{carry: [enum p*], order: [enum w*], new_tasks: [{title: string 1..120, direction: enum g*}] ≤ 3, reason: string ≤ 200}`，`max_tokens: 512` ⚖️；复核：键必须在集合内、`order` 无重复、标题去控制字符后非空。
- **非法建议在提交时被拒、不半应用**：每条提议先过 §11.4 公共的提交前校验（含 `apply_op` 试跑），所以「引用不存在/不在该周的任务的 `ReorderTasks`」「重复顺延」在 `submit` 就被拒、不写任何行（J21，确定断言）；accept 仍是单事务。
- **事件 payload（M2，§2 #25）**：T5.4.1 同时给 `CreateTasks` 与 `CarryOverTask` 的 `task.created` payload **只加字段** `direction_id`（`CarryOverTask` 取源任务的归属），让 §11.2.1 的重放规则第 2 条对 propose 产出的任务成立。
- **已知小瑕疵**：先批 reorder、再批 carry，顺延进来的任务 `sort_key = 0` 与排第一的并列（R9）。
- **去重只认 AI 自己产出的提议（T5.4.1 实现时补，统筹设计澄清，第 2 轮 M-2）**：§11.4 公共「去重」检查的「仍然有效的挂起提议」范围限定为**这个能力自己产出的**——判据是 `sin90_ai_calls.proposal_id` 关联且 `task_kind = propose`（§11.4 公共「提议形状」原文：「是不是 AI 产出以 `sin90_ai_calls.proposal_id` 关联为准，J24」），**不是** `id` 前缀（`"ai-propose-<ulid>"` 只为人类可读，人类或 automation key 经 `POST /proposals` 直接提交时可以自选任意 id，包括恰好长得像这个前缀的）。人类或 automation key 直接提交的同形状提议（哪怕 carry/reorder/create 形状、覆盖范围都一样）**永远不挡** AI 的去重判断——按定义它们不在 `sin90_ai_calls` 里留痕。
- **三类去重的粒度不同**：carry 是「按任务剔除」——已被某条仍然有效的挂起 carry 提议覆盖的源任务从候选集里去掉，其余未覆盖的源任务照常候选；reorder 与 create 是「整类跳过」——一旦判定为仍然有效，本轮直接不产出、不提交，不看候选内容。
- **去重的 reorder 「仍然有效」判据（T5.4.1 实现时补，统筹设计澄清）**：不能只看 `AiSink::precheck` 的试跑结果——`precheck` 只验证挂起提议引用的任务确实在该周（关系约束），一个只覆盖 W 部分非终态任务的**旧**排序提议也能通过试跑，但已经不是「当前该怎么排」的正确答案。因此「仍然有效」在 precheck 通过之外，**额外要求**该挂起提议的 `order`（作为集合）恰好等于 W 当前全部非终态任务的 id 集合；**多一个、少一个都不算**「仍然有效」，会被当作过期提议放行本轮重新产出。这条判据**只看集合本身**，不看集合内任务的状态变化——一个仍在挂起提议 `order` 里的任务哪怕状态从 `planned` 变成了 `in_progress`（只要没离开非终态集合），并不会单独让这条挂起提议失效，这是有意接受的（§11.9 残余风险的同一类取舍：判据成本与精确度的折中）。
- **去重的 create 「仍然有效」判据（T5.4.1 实现时补，统筹设计澄清，第 2 轮 M-2）**：precheck 通过之外，**额外要求**该挂起提议引用的**每一个** `direction_id` 仍然是**当前**的缺口 Direction——非终态、当前 rhythm 配额 `pct > 0`、W 里仍然没有它的任务；三者任一不满足（最典型：Direction 被 abandon/achieved），这条挂起提议就不再算「仍然有效」，本轮可以重新产出。
- **一步多提议的 token 记账（T5.4.1 实现时补，统筹设计澄清）**：propose 一次决策（一次模型调用或一次 reflex 兜底）可能同时拆成 carry/reorder/create 至多三条独立提议，各自铸造自己的 `sin90_ai_calls` 行；若三行都各自完整记录 `prompt_tokens`/`completion_tokens`/`latency_ms`，会把**本 run 这一次模型调用的用量**重复计 2-3 遍——正确规则是**本 run 所有模型调用用量之和，每次调用只计一次**。做法：三条里**只有第一条被尝试提交的**（carry 优先于 reorder 优先于 create，按本节枚举顺序；因内容为空或被去重跳过而未被尝试的不算「尝试」）保留真实的 `prompt_tokens`/`completion_tokens`/`latency_ms`；其余兄弟行一律置 `NULL`/`NULL`/`0`，无论该行最终是 `ok=1` 还是被 `submit` 拒绝——即便发生部分被拒（`ok=1` 与 `rejected_by_precheck` 混合）的情况，也按同一条「首行保留、余下清零」规则处理，不因某一行被拒而重新指定谁是「首行」。按 `run_id` 对 `sin90_ai_calls.prompt_tokens` 求和应等于该次模型调用的真实用量，不是它的整数倍。**被拒首行的落地同样依赖 `record_call` 的尽力而为语义，但不是 R6 本身**（T5.4.1 实现时补，2026-09-24 review L-e）：§11.3.5 的 R6 限定的是**非产出步**的 `record_call`；被拒的首行原本是一次**真产出**的决策，只是 `submit` 的试跑事后判它不能落地，才被改写成失败记录——这里共享的是 R6 同一条「`record_call` 失败只 `warn!`、不阻断 run」的姿态，不是 R6 本条判据的适用范围本身。这一行万一因为 `record_call` 自己失败而丢失，是已知且接受的降级，不是这条 token 规则的例外。

**决策已经产出、但三类都没有被尝试**（T5.4.1 实现时补，2026-09-24 review L-e，按代码真实条件表述）时，记恰好一行 `ok=1`、`proposal_id = NULL`、**保留** `prompt_tokens`/`completion_tokens`/`latency_ms`（真实值，不清零——它是且仅是这次决策的唯一一行，谈不上「首行/兄弟行」）。「三类都没有被尝试」不等于「三类各自内容都是空的」——三个原因各自独立、可以任意组合：carry 本身无候选可挑、reorder 算出来和 W 当前顺序相同、create 本身没有新任务；**或者** reorder/create 被去重整类跳过（`dedup.skip_reorder`/`dedup.skip_create` 为真）。只要三类各自因为其中某个原因都没有走到提交这一步，就记这一行。

这与 L-4 的短路场景不同（且更早发生）：L-4 是**去重后 carry 候选为空、且 `skip_reorder`、`skip_create` 都为真**这三个条件**同时**成立时，`run_propose` 在决策产生**之前**就直接返回结果——不跑引擎梯、不调模型、连这一行 `ok=1` 的调用记录都不写，是比"决策已产出但都不用提交"更早、更彻底的短路。

### 11.5 结构约束：AI 模块只能提议（T5.1.1）

**端口**（scratch `ports.rs` 已 check + test，含 `tokio::spawn` 证明 run 的 future 是 `Send`）：

```rust
pub trait ModelPort: Send + Sync {
    fn complete(&self, req: ModelRequest) -> impl Future<Output = Result<ModelReply, ModelFailure>> + Send;
}
/// ai/ 的全部写能力：只有这两个方法。由 Sin90Store 在 store/ai_port.rs 实现。
pub struct ProposalDraft { pub id: String, pub ops: Vec<Sin90Op>, pub rationale: Option<String> } // 没有 source / status
pub trait AiSink: Send + Sync {
    /// 一个 BEGIN IMMEDIATE：allowed_ops → validate → SAVEPOINT 试跑 apply_op → ROLLBACK TO
    /// → source = source_for(rec.engine, rec.served_tier) → INSERT 提议 + 事件行 + ok=1 调用行 → COMMIT
    fn submit(&self, cap: Capability, draft: ProposalDraft, rec: AiCallRecord)
        -> impl Future<Output = Result<(), SinkError>> + Send;
    /// 只写非产出的尝试
    fn record_call(&self, rec: AiCallRecord) -> impl Future<Output = Result<(), SinkError>> + Send;
    /// 批量「挂起提议是否仍有效」：一个 BEGIN IMMEDIATE，每条一个 SAVEPOINT 试跑，最后整体 ROLLBACK
    fn precheck(&self, cap: Capability, drafts: &[ProposalDraft]) -> impl Future<Output = Vec<bool>> + Send;
}
pub trait SettingsRead: Send + Sync {
    fn settings(&self) -> impl Future<Output = Result<AiSettings, ReadError>> + Send;
}
/// store 实现走独立的读取器池：与写池同一目标的连接选项 + pragma("query_only","ON")：写即报错
pub trait AiReadModel: SettingsRead {
    fn inbox(&self, limit: u32) -> impl Future<Output = Result<Vec<Task>, ReadError>> + Send;
    fn direction_candidates(&self, limit: u32) -> impl Future<Output = Result<Vec<DirectionCandidate>, ReadError>> + Send;
    fn title_history(&self, normalized: &str) -> impl Future<Output = Result<Vec<DirectionId>, ReadError>> + Send;
    fn review(&self, id: &str) -> impl Future<Output = Result<Option<Review>, ReadError>> + Send;
    fn week_tasks(&self, week_id: &WeekId) -> impl Future<Output = Result<Vec<Task>, ReadError>> + Send;
    // T5.3.1/T5.4.1 各自再加：weekly_draft(week)、done_titles(week)、previous_open_week(week)、rhythm_alloc()
    // （「挂起提议是否仍有效」要试跑写，只能在 AiSink::precheck 上，不在这里）
}
```

「挂起提议是否仍有效」要重跑提交前校验，而试跑要写（再回滚）——读取器池做不到，所以它只在 `AiSink::precheck` 上（批量，见 §11.4 公共「去重」），不在 `AiReadModel` 上。它不改变任何行，J8 覆盖。

**读取器怎么建（v2.1 M1）**：`Sin90Store::ai_reader()` 用**与写池同一目标**的 `SqliteConnectOptions` 克隆加 `.pragma("query_only", "ON")` 建一个独立小池（2 连接 ⚖️）。`query_only` 对文件库与共享缓存内存库都拒写（`read_only(true)` 在共享缓存下挡不住，评审实测）。测试模式下 `Sin90Store::open_memory()` 改为按实例命名的共享缓存内存库 `file:sin90-<ULID>?mode=memory&cache=shared`（写池 1 连接），读取器连同一个名字，才能读到同一份数据；普通 `sqlite::memory:` 的第二个池会连到另一个空库。共享缓存是表级锁：读取器遇到另一任务未提交的写事务会等待（sqlx 的 unlock_notify）到其提交；同一任务在持有写事务时去读会自锁——ai run 不会这样做（sink 的方法都是自带事务的原子调用，run 不持有事务）。scratch `readonly.rs` 三条测试覆盖：文件 WAL 与共享缓存下「读得到、写即 `readonly` 错、写池照常写」，以及「读取器等另一任务提交后成功」。

**依赖方向**：`ai/` 只 `use crate::core::*` 与 `crate::ai::*`；`store/ai_port.rs` 实现 `AiReadModel + AiSink`（store → `ai::ports`，后者只含 trait 与值类型）；`adapter_agent24/clients/model.rs` 实现 `ModelPort`；`http/` 组装三者、起后台 run、补发镜像事件。`ai/` 里的函数全部对三个 trait 泛型，**不出现具体类型**。

**三层判据**：
1. **syn 白名单结构测试** `ai_boundary`（J7，H2）：Sin90 加 dev-dependency `syn = { version = "2", features = ["full", "visit"] }`。测试遍历 `src/ai/**/*.rs`，按文件位置算模块深度（`ai/mod.rs` = 1，`ai/x.rs` = 2，内联 `mod` 再 +1），对每个文件 `check_source(src, depth)` 必须为 `Ok`（scratch `boundary.rs`）。规则：
   - 展开全部 use 树（分组、重命名、glob），访问所有表达式/类型/模式路径、宏路径，以及宏 token 流里的 `a::b` 链（`format!("{:?}", crate::store::X)` 也会被看见）；注释与字符串字面量由 syn 天然排除。
   - 路径根是 `crate`/`sin90` → 第二段 ∈ {`core`, `ai`}；根是 `super` → 数出 `super` 个数 k，k 到达 crate 根时下一段 ∈ {`core`, `ai`}，超过 crate 根即拒；根是 `self` 且后面跟 `super` → **先剥掉开头的 `self` 再按 `super` 规则判**（v2.1：`use self::super::super::store::…` 被拒）；其余 `self::…`/`Self::…` 在本模块内，放行。
   - `std`/`core`/`alloc` 下的 `fs`、`process`、`net`、`os` 一律拒（v2.1 M5：ai 模块不直接碰文件、子进程、网络、平台接口，含 `std::os::unix::net`）。
   - use 路径的根只能是上面几种或外部 crate 白名单（`std core alloc serde serde_json thiserror tracing sha2 hex`）；带前导 `::` 的只能是外部 crate 白名单。
   - 非 use 的多段路径，根还可以是本文件的局部名（use 绑定的名字、本文件定义的条目、泛型参数）或 prelude/原始类型（`String::new`、`u32::MAX`）——这些名字本身已经过 use 检查或是本地定义。
   - `extern crate`、`#[path]`、`include!` 一律拒。
   - **`#[cfg(test)]` 条目跳过检查**（v2.1 M4，写死）：属性**恰好**是 `#[cfg(test)]` 的条目（通常是 `mod tests`）不检查，单元测试可以用真实 store 建夹具；`#[cfg(any(test, …))]` 等其它 cfg 照常检查。备选「测试一律放 `tests/`」不采用——它会把 ai 内部私有函数的单元测试逼成公开接口。
   - 正对照 29 条（scratch `positive_controls_each_trip`），含第 1 轮的两种绕法与第 2 轮的 `self::super::super::store`（use 与表达式）、`std::fs`/`std::process`/`std::net`/`std::os::unix::net`/`::std::fs`、`$crate::store`、`r#store`、`<crate::store::X as Clone>::clone`、`cfg(any(test, …))` 下的 store 引用；干净样例含 `self::super::ports`、`tracing::warn!`、`std::time`。第 1 轮的两种绕法：`use crate::{core::Sin90Op, http::Sin90State};` + `s.store.update_review_body(..)`；`let u = "http://x"; use crate::store::Sin90Store;`。
   白名单信任一个前提：`core` 与 `ai` 不再导出 store/http 的东西（`core` 零 I/O 依赖是 §5.2 的既有层规则）。
2. **类型与连接层**：`ai/` 的一切写都只能经 `AiSink` 的方法；`AiReadModel` 的实现拿的是 `query_only` 读取器池，哪怕 store 侧有人在读路径里顺手调了 `attention_apply_new_events` 也会直接报错——文件模式与测试用的内存模式都成立（上文「读取器怎么建」）。
3. **行为级表快照**（J8，M8）：在夹具库上用桩模型把三个能力各跑一遍，比较运行前后除以下**写死的三项**之外所有表的全部行，必须逐字节相同：`sin90_proposals`、`sin90_ai_calls`、`sin90_events WHERE entity = 'proposal'`。`sin90_attention_daily` / `sin90_attention_watermark` **不排除**：它们是派生投影，AI 读路径若去折叠它（读时写），J8 就应当变红——那是真回归。正对照：随后用人类 key accept 其中一条 → 快照必变。

**为什么不拆 crate**：真正的编译期保证要把 `core` 与 `ai` 拆成独立 crate；今天 `core/store/http` 是一个 crate，拆分与 M5 无关，留给 TS.1.1 评估。v1 以「syn 白名单 + 只读池 + 行为快照」组合代替，并如实称为「结构测试」。

### 11.6 存储改动（§2 #20/#21，§4.1 已登记）

一个迁移文件（编号取当时 max+1，spec.md「不预分配」）：

```sql
ALTER TABLE sin90_ai_calls ADD COLUMN run_id            TEXT;
ALTER TABLE sin90_ai_calls ADD COLUMN proposal_id       TEXT;     -- 不加 FK：失败行没有提议
ALTER TABLE sin90_ai_calls ADD COLUMN served_tier       TEXT;     -- local|remote|NULL，代码约束
ALTER TABLE sin90_ai_calls ADD COLUMN model_id          TEXT;
ALTER TABLE sin90_ai_calls ADD COLUMN prompt_tokens     INTEGER;
ALTER TABLE sin90_ai_calls ADD COLUMN completion_tokens INTEGER;
ALTER TABLE sin90_ai_calls ADD COLUMN error_kind        TEXT;
CREATE INDEX idx_sin90_ai_calls_run      ON sin90_ai_calls(run_id);
CREATE INDEX idx_sin90_ai_calls_proposal ON sin90_ai_calls(proposal_id);
CREATE TABLE sin90_settings (
    key        TEXT PRIMARY KEY,
    value      TEXT NOT NULL,          -- JSON 标量
    updated_at TEXT NOT NULL
);
```

事件：`setting.changed`（`entity = setting`、`entity_id = <key>`）是本补丁唯一新增的非 Op 事件；`task.direction_assigned` 由 Op 产生（§11.2.1）；`CreateTasks`/`CarryOverTask` 的 `task.created` payload 加字段 `direction_id`（§2 #25，不改 schema）。

### 11.7 判据（每条带正对照；`cargo test <过滤>` 先 `-- --list` 断言匹配数 > 0；新回归测试一律变异验证）

**T5.1.1 引擎梯 + 调用记录**

| # | 测试（过滤名） | 断言 | 正对照 / 变异 |
|---|---|---|---|
| J1 | `ai_ladder_local_unavailable_degrades_to_reflex` | classify 一条、R1 无结论、local 桩回 `unavailable/no_provider`、R2 有结论 → 恰好 3 行：`reflex ok=0 undecided`、`local ok=0 unavailable.no_provider`、`reflex ok=1 fallback_from=local proposal_id=<p>`；提议 `source = rule` | 桩改回成功 → 2 行、`source = local_brain`、无 `fallback_from` |
| J2 | `ai_ladder_abort_` | `ConnectionLost` / `GenerationEnding` / `Cancelled` 各一：run `aborted`、0 条提议、之后没有 reflex 行、后续条目不再执行 | 同一位置换 `Timeout` → 降级并产出 reflex 提议 |
| J2b | `ai_ladder_defer_` | `Busy` / `RateLimited` / `NotReady` 各一：当前与剩余条目 `deferred`、**没有** R2 行、模型桩总共只被调用 1 次 | 换 `Unavailable/request_rejected` → 本条降级到 R2，下一条目照常调用模型 |
| J2c | `ai_ladder_budget_and_circuit` | (a) 预算 = 1 → 第二条目 `deferred`；(b) 第一条目 `no_provider` → 第二条目不再调用 local（桩计数 1），直接 R2 且 `fallback_from=local` | (b) 换 `timeout`（不熔断）→ 第二条目仍调用 local |
| J3 | `ai_ladder_failure_table` | 对 `ModelFailure` 每个变体断言 `action()`/`opens_circuit()` 与 §11.3.4 一致；穷尽 `match`（新增变体不改测试就编译失败） | 变异：`GenerationEnding` 改成 `Degrade` → 红 |
| J4 | `ai_ladder_executive_gate` | `plan()` 在 `{LocalOnly, RemoteAllowed} × {开, 关} × {port 有, 无}` 八格上，`Model(Executive)` 只在 (RemoteAllowed, 开, 有) 出现 | 变异：删掉 `settings.executive_enabled` 条件 → 红 |
| J5 | `ai_ladder_served_tier_decides_source` | 请求 executive、桩回 `tier: local` → store 推导 `source = local_brain`、`served_tier = local` | 桩回 `tier: remote`（开关开）→ `source = executive` |
| J6 | `ai_ladder_privacy_tripwire` | 计划时开关开、回复前用户关掉、桩回 `tier: remote` → 该步 `ok=0 privacy_tripwire`、结果丢弃、降级 | 开关保持开 → 同一回复被采用 |
| J7 | `ai_boundary` | `src/ai/**/*.rs` 每个文件 `check_source(src, depth)` 为 `Ok`；检查器自身的 29 条正对照全部触发、干净样例通过；`#[cfg(test)] mod tests` 里引用 store 不触发，`#[cfg(any(test, …))]` 里引用 store 触发 | 变异：在 `src/ai/mod.rs` 加 `use crate::{core::Sin90Op, http::Sin90State};` → 红（PR body 记录） |
| J8 | `ai_boundary_tables_unchanged` | §11.5 第 3 层：三个能力各跑一遍，排除写死的三项后逐字节不变（`sin90_attention_*` 在比较范围内） | 随后人类 accept 一条 → 快照变化被检出；变异：在 `AiReadModel::week_tasks` 实现里调 `attention_apply_new_events` → 读取器报 `readonly` 错 / J8 红——**测试跑在 `open_memory`（命名共享缓存）上，变异必须在该模式下触发**（v2.1 M1；scratch `readonly.rs` 证明 `query_only` 在该模式下拒写） |
| J9 | `ai_calls_link_integrity` | 一次 run 的所有行 `run_id` 相同；每条 AI 提议恰好一行 `ok=1 AND proposal_id = 它`；`ok=0` 的行 `error_kind` 非空 | 注入 `submit` 在插调用行后失败 → 提议行也不存在（同事务） |
| J10 | `model_client_` | (a) `complete` 走 `call_with_timeout(125s)`、不带 `request_id`；(b) `-32000 {kind: unavailable, retryable: false, cause: backend_config}` → `ClientError::Unavailable{false, BackendConfig}`；(c) cause 不在闭集 → `Other`；(d) `cancelled` → `Cancelled`；(e) 计数 18；(f) `usage` 映射到 `ModelReply` | (b) 正对照：`retryable: true, cause: no_provider` → 另一值且 `is_retryable()` 为真 |
| J10b | `settings_ai_` | `PUT /settings/ai` 自动化 key → 403 且无事件；人类 key → 200、`setting.changed` 恰好 1 条；未知字段 → 400 | 缺行时 `GET` 返回 `false`（删掉缺省分支 → 红） |
| J10c | `manifest_official_is_local_only` | 解析正式 `domain-os.yml`：`kernel_capabilities` 含 `models`、`requires_models == []`、`model_access` 缺省或 `local_only`；不开 feature 编译时 `MODEL_ACCESS == LocalOnly` | 正式 yml 写入 `model_access: remote_allowed` → 红（硬约束的钉子） |

**T5.2.1 classify**

| # | 测试 | 断言 | 正对照 / 变异 |
|---|---|---|---|
| J11 | `classify_stub_proposes_and_data_unchanged` | inbox 一条 + 两个 Direction，桩选 `d1` → 恰好 1 条 `pending` 提议，`ops == [AssignTaskDirection(t, D1)]`，`source = local_brain`；任务 `direction_id` 仍为 NULL；人类 accept → `/today` inbox 不再含 t，`task.direction_assigned` 恰好 1 条、payload 含 `area_id` | 自动化 key accept → 403，任务仍在 inbox |
| J12 | `classify_rejects_invented_keys` | 桩回 `"d9"` / 一个真实 ULID / 多余字段 → 该步 `bad_output`，降级 | 桩回 `"d2"` → 采用 |
| J13 | `classify_reflex_history_short_circuits` | 已有同标准化标题的已归类任务 → 提议 `source = rule`，模型桩**被调用即 panic** | 标题改一个字 → 模型被调用 |
| J14 | `classify_dedup_only_valid_pending` | 第二次 run 跳过有有效挂起提议的条目 | (a) 人经另一条提议先把它归类；(b) 把目标 Direction 转成 abandoned——两种情况下旧挂起提议都不再挡，条目被重新处理 |
| J14b | `classify_task_ids_over_limit` | `task_ids` 21 个 → 400，不产生 run | 20 个 → 202 |
| J15 | `assign_task_direction_validate_` / `_apply_` | §11.2.1 A1–A5 各一正一反（scratch `ctx::tests`）；批内 `[Assign(t,d1), Assign(t,d2)]` 拒；apply 层：挂起期间任务被归类 → accept 422、无事件、提议仍 `pending` | 未被抢先 → 200 |
| J16 | `classify_remote_down_local_up` | `RemoteAllowed` + 开关开，桩：`complex` → `unavailable/no_provider`，`simple` → 成功 `tier: local` → `source = local_brain`；调用行 `executive ok=0`、`local ok=1 fallback_from=executive` | 本地桩也失败 → `source = rule` 或无提议，且 `local ok=0` 行存在 |

**T5.3.1 summarize**

| # | 测试 | 断言 | 正对照 / 变异 |
|---|---|---|---|
| J17 | `summarize_numbers_come_from_draft` | 固定事件夹具 → 周草稿；桩叙述用 `{{fN}}`/`{{tN}}` → 提议正文包含 `render_facts(草稿)` 原文；正文的**每个数字串都出现在 `render_facts` 输出或代入的任务标题里**（L2） | 负对照（每条都让模型步 `bad_output`、提议来自 reflex（`source = rule`），除非注明）：① `编码 99 小时`、全角 `９９`、`Ⅻ小时`、`&frac12;`；② **标签错位**「编码投入达到{{f2}}」→ 不拒（R1），断言 f2 的数值只出现在 `〔f2 的标签：值〕` 单元内；③ **仿造数字块**「## 本周数字（修正）…」、`本​周数字`（零宽）、`本　周数字`（全角空格）；④ **手写单元**「〔领域「Coding」投入：十八 小时〕」「〔…：翻倍〕，〔完成任务数：全部〕」「「Coding」很忙」；⑤ **量词绕过**「十八 小时」「十八​小时」「十八　小时」「拾捌小时」「{{f3}}小时」「{{f3}} 小时」「{{f3}}​小时」；⑥ **切行绕过**单独 `\r`、U+2028、U+2029；⑦ **占位键**`{{t+1}}`、`{{t01}}`、`{{ f1 }}`、`{{f99}}`；⑧ **标题注入**：标题含 `\r## 伪造标题`、`**加粗** [链接](…) <b>` 与 `〔假：十八小时〕` → 不拒，断言渲染结果单行、`#`/`*` 被转义、`<` 成全角、不含 `〔假`；⑨ 行首 `##`/`-`/`>`/`|`/`<h>` 的叙述 → 不拒，渲染后没有任何行以块语法或 `<` 开头。scratch `round2_bypasses_are_rejected`（25 条）+ `titles_are_sanitized_when_substituted` + `markdown_and_html_flattened`。变异：删掉 `RESERVED` 检查 / `normalize` / 控制字符检查 / `sanitize_inline` → 各自对应断言变红 |
| J18 | `draft_review_body_validate_` / `_cas_` | D1–D7 各一正一反；挂起期间人类 `PATCH` 正文 → accept 422 `StaleBase`、正文仍是人写的 | 无人改 → accept 200、正文 == 提议正文、`review.updated` 恰好 1 条、payload 形状与人类路径相同 |
| J19 | `summarize_preconditions` | finalized → 409；daily → 400；正文含人写文字（含「数字块下多一行」、旧周数字块，即①②都对不上）→ 不产提议、run 结果 `skipped: human_text`；定稿后再 accept 旧提议 → 422 `ReviewNotDraft` | 正文为空、或逐字等于 ①`render_facts(当前草稿)` 或 ②`render_weekly_draft_markdown(当前草稿)`（T4.3.2 自动草稿）之一 → 产出提议；C1 端到端正对照：review 节律到点自动建草稿（正文=②）→ 触发 summarize → 直接产出提议，不被误判为人写 |

**T5.4.1 propose**

| # | 测试 | 断言 | 正对照 / 变异 |
|---|---|---|---|
| J20 | `propose_proposals_submit_and_apply` | P(active) + W(planning) 夹具 → 至多 3 条提议，全部通过提交前校验；逐条人类 accept 成功；`CreateTasks` 与 `CarryOverTask` 产生的 `task.created` payload 含 `direction_id` | 把 P 转成 reviewing 后再触发 → 无 carry 提议 |
| J21 | `propose_invalid_rejected_at_submit` | 构造 `[ReorderTasks(W, [W 的真实任务, 不存在的任务])]` → `AiSink::submit` 返回 `SinkError::Invalid`；**库逐字节不变**（无提议行、无调用 `ok=1` 行、无事件、`sort_key` 未变）。同样断言：顺延一个已被顺延过的任务（撞 `idx_sin90_task_carried`）→ submit 拒 | 去掉不存在的任务 → submit 成功、仅新增提议行 + 事件行 + 调用行、`sort_key` 仍未变（试跑已回滚） |
| J21b | `apply_op_has_no_non_db_side_effects` | 结构断言：用 syn 解析 `src/store/repo.rs` 中 `apply_op` 的函数体（与它调用的本文件辅助函数），所有路径的根都在白名单内——`sqlx`、`serde_json`、`crate::core` 类型、本文件的 `append_event`/`read_*`/`require_*`/`allocate_*`/`to_wire`/`from_wire`/`now_iso8601`/`ulid`；出现 `std::fs`、`tokio::fs`、`write_markdown_atomic`、任何 `adapter_agent24`/`http` 路径或 `emit` 即失败（写 `sin90_outbox` 行是数据库写，允许）；另一条行为断言：在临时 `data_dir` 下对每种 Op 调一次 `submit`（只试跑），目录内容前后一致 | 变异：在 `apply_op` 某分支里加一行 `std::fs::write` → 两条都红 |
| J22 | `ai_allowed_ops_per_capability` | 以 propose 身份提交含 `AssignTaskDirection` 的草稿 → `SinkError::Invalid`，无行；三个能力的允许集各一正一反 | 变异：`allowed_ops` 返回全集 → 红 |

**T5.5.1 真实挂载**

| # | 判据 | 期望 | 正对照 |
|---|---|---|---|
| J23 | 断网 classify | **包 A**（正式构建，`local_only`）：`OMLX_URL` 指本地 Python 桩、无远端 → classify 产出 `source = local_brain` 提议。**包 B**（`--features remote-allowed-manifest` 构建，安装 `domain-os.remote-allowed.yml`，开关开）：`OLLAMA_URL` 指一个**被内核标成 Remote 且连不上**的地址（ME4-S2 §2.3：`http://[::ffff:127.0.0.1]:<关闭的端口>`）、`OMLX_URL` 指本地桩 → 仍产出 `source = local_brain` 提议；调用表里**存在 `engine='executive'` 的行**（证明 executive 路径真的被走到，不是空转），产出行 `served_tier = local` | 停掉本地桩 → 无 `local_brain` 提议，出现 `unavailable.no_provider` 行 |
| J23b | 编译期常量与随包 manifest 一致 | 每个被安装的包：`bin/sin90 print-model-access` 的输出 == 解析已安装 `domain-os.yml` 的 `model_access`（缺省按 `local_only`） | 用包 B 的二进制配包 A 的 yml → 断言失败 |
| J24 | 来源一致 | `SELECT count(*) FROM sin90_proposals p JOIN sin90_ai_calls c ON c.proposal_id = p.id AND c.ok = 1 WHERE p.source NOT IN ('local_brain','executive','rule') OR p.source != CASE WHEN c.engine = 'reflex' THEN 'rule' WHEN c.served_tier = 'remote' THEN 'executive' ELSE 'local_brain' END` = 0，且 AI run 期间产生的每条提议都能 join 到恰好一行 | 在库的副本里改掉一行 `source` → 同一查询 = 1 |
| J25 | AI 期间无直写 | 挂载模式经 daemon 真实端口触发三个能力，跑 J8 的表快照 | accept 正对照。注：tasks.md 原句「直写路由调用数 0」在进程内 AI 下恒真，不构成判据，故以表快照代替 |
| J26 | 真 oMLX 冒烟 | `#[ignore]` 手动：`~/.omlx/models` 下的模型，三个能力各一次，记录 `model_id`、延迟、`bad_output` 率**及其按 `NarrativeError` 分类的构成**（尤其「汉字数词 + 量词」误杀引起的降级比例，v2.1 M3）、classify 各置信度档的命中情况（给 Q8 定阈值） | —— |

### 11.8 自审

- **两个 Op 的范围是否过窄**：`AssignTaskDirection` 只收 inbox、`DraftReviewBody` 只收 draft——故意的；放宽以「加 CAS 字段」的方式做（§2 #17 复审触发条件）。
- **`ValidationCtx` 又加宽了**：推翻了 §3.3「唯一一次」；T5.2.1 一次加齐。
- **提交前校验只给 AI 路径**（F-2 / R7）：不对称，但改 HTTP 路径会改变已测行为。v2 的试跑让 AI 路径的提交前校验与 accept **同一份代码**，而不是另写一个「近似校验」。
- **v2.1 的叙述复核是否又开了新口子**：规范化只删 Cf、压空白，不做 NFKC（NFKC 会把全角数字变半角——那本来就被 `is_numeric` 拒了，不需要；但也意味着兼容字符组合不被折叠，列入 Codex 补审）；`〔〕「」` 禁用让「手写单元」整类消失，而不是逐个补内容规则。
- **试跑的代价**：每次 `submit` 在写锁下多执行一遍 `apply_op`；三个能力的提议都是个位数 op，代价可忽略；它换来的是 J21 从「两可」变成确定。
- **「executive」被如实降格，且正式包里不可达**：§11.3.2 写明原因（内核只按 manifest 管隐私），没有把用户开关写成隐私保证。
- **数字保证的措辞**：§11.4.2 末尾只声称机制做得到的部分，并点名了「单元前后文可以矛盾」这一条没挡住。
- **判据是否会空转**：J3 穷尽 `match`；J7 带 29 条正对照；J8/J25 有 accept 正对照且排除清单写死；J13 用「被调用即 panic」的桩；J21 断言库逐字节不变；J23 断言 `engine='executive'` 行存在；J23b 反配二进制与 yml；J24 在篡改副本上变红。
- **scratch 覆盖了什么**：两个 Op 的 wire 形状与校验矩阵（含批内叠加）；梯的计划、三种动作、熔断、预算、绊线（含途中关开关）、来源映射、产出行随 `Outcome` 返回；run future 的 `Send`；classify 的 schema 与复核；summarize 的原子单元、行首剥离、各负对照与 J17 数字串判法；syn 白名单检查器；sqlx SAVEPOINT 试跑。**没覆盖**：真实 `build_snapshot`/`apply_op` 的 SQL、HTTP 路由、adapter 的 `ClientError` 扩展、只读连接池——要改真实 crate，属于实现。

### 11.9 残余风险（接受，写明谁来盯）

| # | 风险 | 为什么接受 / 缓解 |
|---|---|---|
| R1 | 叙述的**前后文**可与数值单元矛盾（「编码投入达到〔领域「Business」投入：…〕」）；不在 `MEASURE` 表里的数量表达（「近半」「翻倍」「seven」「十来个人」）能漏过 | 数值本身永远带正确标签出现，矛盾对读者可见；数字块由程序生成是硬保证。J26 冒烟里人工看叙述 |
| R2 | syn 白名单的已知盲区：`core`/`ai` 若再导出 store/http 的东西；宏展开后的代码不可见（只扫 token 里的 `a::b` 链），`macro_rules!` 用 `$a:tt`/`$m:ident` 把路径拆开拼接看不出来；本地定义一个同名 `fn sqlx` 后 `sqlx::…` 会被当成局部名放行；字符串形式的路径（`#[serde(deserialize_with = "crate::store::…")]`）不是语法路径，看不到 | `core` 零 I/O 是 §5.2 既有层规则；J8/J25 行为快照与只读读取器兜底（任何真写都会被发现）；编译期保证要拆 crate（TS.1.1 评估）；列入 Codex 补审重点 |
| R3 | 测试包 B 的 `local` 调用在本地不可用时会被内核送到远端 | 只存在于测试包；正式包不声明 `remote_allowed`（J10c 钉住）。Agent24 侧「逐次只能收窄」followup 由统筹登记 |
| R4 | summarize 的数字在提议挂起期间变旧 | 人重新触发；v1 不在 accept 时重算 |
| R5 | 没有拒绝路由，过期提议永久 `pending` | 去重只看「仍有效」的挂起提议；拒绝路由见 🟡 Q6 |
| R6 | 非产出步的调用记录写失败只 `warn!` | 产出行与提议同事务（M3），「每条提议有来源记录」是保证；失败行的缺失只影响降级统计 |
| R7 | HTTP `POST /proposals` 不做提交前校验（F-2） | 已登记，不在 M5 范围；accept 时两条路径一致 |
| R8 | 人类 `PATCH /reviews/{id}` 没有正文上限，AI 路径有 64 KiB | 不同入口不同上限；人类路径另议 |
| R9 | reorder 与 carry 分开批时顺延任务 `sort_key = 0` 与首位并列 | 显示顺序小瑕疵 |
| R10 | 本地模型不遵守 `json_schema` 时模型步总是 `bad_output` | 降级到 reflex，不丢功能；J26 暴露遵守率 |
| R11 | run 状态只在内存 | 调用记录与提议持久；run 只是观察窗口 |
| R12 | 候选截断（40 个 Direction、50 个标题）可能漏掉正确答案 | ⚖️ 值 |
| R13 | 延后（Busy/RateLimited）后本 run 剩余条目都不处理，用户要再点一次 | 有意：弱规则不替代模型；`GET /ai/runs/{id}` 列出 `deferred` 条目 |
| R14 | 叙述与任务标题里的**行内**链接 `[x](javascript:…)`、自动链接不在 `fill_narrative` 里处理（任务标题里的 `[]()` 已被转义，模型叙述里的不转义） | 渲染端契约：任何渲染复盘正文的前端（Pet0、Web UI）必须用禁 raw HTML、只放行 `http`/`https` 链接的安全 Markdown 渲染；Sin90 的 `.md` 导出是纯文本文件，不执行 |
| R15 | 「汉字数词 + 量词」规则误杀日常写法（「这一周」「一个」「一次」） | 提示词明示改写法；J26 统计误杀引起的降级比例，比例过高时再收窄规则（例如只对 `小时/分钟/%/倍` 生效） |

### 11.10 交给实现的接口清单（按 T5.x）

**T5.1.1 引擎梯 + 调用记录**
- `src/ai/mod.rs`、`src/ai/ports.rs`：`Capability`、`Engine`、`ServedTier`、`ModelAccess`、`AiSettings`、`Complexity`、`ModelRequest`、`ModelReply`（含 usage）、`UnavailableCause`、`ModelFailure{…}::action()/kind_str()/opens_circuit()`、`LadderAction{Degrade, Defer, Abort}`、`ModelPort`、`ProposalDraft`、`AiSink{submit(cap, draft, rec), record_call, precheck}`、`SettingsRead`、`AiReadModel`（首批方法）、`AiCallRecord`、`SinkError`、`ReadError`。
- `src/ai/ladder.rs`：`plan()`、`RunState`、`MAX_MODEL_CALLS_PER_RUN`、`RUN_DEADLINE_SECS`、`run_item()`、`source_for()`、`tripwire()`、`Outcome{Produced{value, engine, rec}, Nothing, Deferred, Aborted}`。
- `src/store/ai_port.rs`：`impl AiReadModel for AiReader`（`Sin90Store::ai_reader()`：同目标连接选项 + `pragma("query_only","ON")`；`open_memory()` 改为按实例命名的共享缓存内存库）、`AiSink::precheck`（批量，一个事务、每条一个 SAVEPOINT、整体 ROLLBACK）、`impl AiSink for Sin90Store`（allowed_ops → validate → SAVEPOINT 试跑 `apply_op` → ROLLBACK TO → 推导 source → 插提议/事件/调用行，一个 `BEGIN IMMEDIATE`）。
- 迁移（max+1）：§11.6。
- `src/adapter_agent24/clients/model.rs`：`ModelClient::new(&Arc<KernelClients>) -> Option<Self>`（前缀 `_a24/model/`）、`complete(&ModelRequest) -> Result<ModelReply, ClientError>`（`call_with_timeout(125s)`，不带 `request_id`）、`impl ModelPort for ModelClient`；`clients/error.rs`：`Unavailable{retryable, cause}`、`Cancelled`，`map_rpc_error`、两个谓词、计数测试 17 → 18。
- `domain-os.yml`：`kernel_capabilities` 加 `models`；新文件 `domain-os.remote-allowed.yml`（仅测试包 B）；cargo feature `remote-allowed-manifest`；`const MODEL_ACCESS`（`include_str!` 解析）；子命令 `sin90 print-model-access`。
- HTTP：`GET|PUT /settings/ai`、`GET /ai/runs/{run_id}`；run 注册表（内存 LRU 64）；每能力单飞；进程内模型信号量 2；`submit` 成功后补发 `proposal.submitted`。
- dev-dependency：`syn = { version = "2", features = ["full", "visit"] }`、`proc-macro2`。
- 测试：J1–J10c；`ai_boundary` 检查器（含 `self` 剥离、`STD_DENY`、`#[cfg(test)]` 跳过）。

**T5.2.1 classify**
- `Sin90Op::AssignTaskDirection`；给 `Sin90Op` 加 `#[serde(deny_unknown_fields)]`（F-1）。
- `ValidationCtx` 一次加齐 `direction_status` / `task_direction` / `review_snap`；`Working` 加两张叠加表；`ProposalError` 八个新变体。
- `DbSnapshot` / `build_snapshot` 载入；`apply_op` 分支（`require_task_week_open` + CAS UPDATE + `task.direction_assigned`）。
- `src/ai/classify.rs`：`candidate_keys`、`schema`、`parse`、`normalize_title`、R1/R2；`AiReadModel::{inbox, direction_candidates, title_history}`。
- `POST /ai/classify`（`task_ids` > 20 → 400）。测试 J11–J16。
- tasks.md T5.2.1 的「`source = local_brain`」文字应同步为「`source` 由产出引擎决定（`local_brain`/`executive`/`rule`）」——在规划分支，由统筹改。

**T5.3.1 summarize**（依赖 T4.3.1 的周草稿函数；2026-09-26 review 后按层拆成三个 stacked 分支：`-store` → `-core` → 顶层）
- `Sin90Op::DraftReviewBody`、`MAX_REVIEW_BODY_BYTES`、`body_sha256`；`apply_op` 分支（`review.updated`，payload 同人类路径）——T5.2.1 已随 `AssignTaskDirection` 一并落地。
- `store/weekly_draft.rs`：`weekly_draft_on(conn, week)`/`direction_area_snapshot_on(conn)`（C1 修复：接受任意连接，`Sin90Store::weekly_draft` 与 `record_routine_fire` 共用同一份查询）；`store/attention.rs`：`attention_on(conn, start, end)` 同理抽出。`record_routine_fire` 改为先 `append_event("routine","fired")` 再在同一个 `&mut tx` 上渲染草稿。
- `src/ai/ports.rs`：`SummarizeDraft{week, by_area, by_direction, tasks_done, routines, auto_draft_md}`、`SummarizeBucket{label: Option<String>, minutes}`、`SummarizeRoutineRow{label, fired, completed}`（ai 自己的周草稿词汇，标题已解析，不是 `store::weekly_draft::WeeklyDraft` 本身）；`AiReadModel::{review, weekly_draft, done_titles}`。
- `src/ai/summarize.rs`：`Fact`、`facts`、`render_unit`、`render_facts`、`is_forbidden_control`、`is_format_char`（Cf 码位表）、`normalize`、`sanitize_inline`、`fill_narrative(narr, facts, titles)`、`compose_body`、`is_program_only(body, candidates: &[&str])`（v2.2：多候选，见 §11.4.2 的「可改写条件」）、`digit_runs`、`MEASURE`（`CJK_NUMERALS` 含繁体 兩/貳/參/陸/億 与俗写 廿/卅/卌）；`build_summarize_request`/`summarize_schema`/`parse_summarize_reply`/`select_review`/`run_summarize`。
- `store/ai_port.rs`：`AiReader::weekly_draft`（调 `weekly_draft_on` 取数字 + `resolve_label` 解析标题回退 id + `render_weekly_draft_markdown` 填 `auto_draft_md`）、`AiReader::done_titles`；`allowed_ops(Summarize) = {DraftReviewBody}`。
- `http/ai_runs.rs`：`AiRunItem` 加 `reason: Option<&'static str>`（`"human_text"`/`"dedup"`，2026-09-26 review M2）。
- `POST /ai/summarize`。测试 J17–J19。
- 给 T4.3.1：按 Direction/Area 统计完成任务时按 §11.2.1 的三步规则取归属。

**T5.4.1 propose**
- `src/ai/propose.rs`：reflex 排序与顺延规则、模型 schema 与复核（排列补全、键集）；`AiReadModel::{week_tasks, previous_open_week, rhythm_alloc}`。
- `repo.rs` `CreateTasks`、`CarryOverTask` 分支的 `task.created` payload 加 `direction_id`（只加字段）。
- J21b（`apply_op` 无非数据库副作用）随 T5.4.1 落地——propose 是第一个依赖试跑拦截关系约束的能力。
- `POST /ai/propose`。测试 J20–J22。

**T5.5.1 验收**
- `tests/agent24_mount_blackbox.rs`：包 A / 包 B（feature 构建 + 对应 yml）、Python 模型桩、J23–J25；J26 手动冒烟记录进 PR body。

### 11.11 产品问题

**已由规格/技术约束决定（不再开放）**：

| # | 问题 | 决定 | 依据 |
|---|---|---|---|
| Q1 | 正式包是否声明 `model_access: remote_allowed` | **不声明**（硬约束，§2 #26，J10c 钉住） | acceptance.md M5「不开远端时只用本地模型」：声明后内核不再保证 `simple` 调用留在本机（§11.3.2） |
| Q2 | executive 用户开关默认值 | **关** | tasks.md T5.1.1「用户设置开启」+ 最严缺省；且正式包里 executive 不可达 |
| Q5 | reflex 产出的 `source` | **`rule`** | tasks.md T5.5.1 的 SQL 已含 `rule`；DESIGN §M5 已同步（§6、§2 #23）；tasks.md T5.2.1 文字由统筹同步 |

**仍留给用户拍板（🟡；文中占位均为最保守取值）**：

| # | 问题 | 选项 | 文中占位 |
|---|---|---|---|
| Q3 | AI 的触发频率 / 是否自动触发 | 仅手动；`/capture` 之后自动 classify；复盘 Routine 到点（T4.3.2 建草稿后）自动 summarize；周进入 planning 时自动 propose；定时（如每日一次） | 仅手动 |
| Q4 | 没有合适 Direction 时，classify 要不要提议**新建** Direction（需要解决批内前向引用） | 不要 / 要 | 不要（任务留在 inbox） |
| Q6 | 要不要在 M5 加 `POST /proposals/{id}/reject` | M5 加 / 以后 | 以后 |
| Q7 | summarize 能不能对**人写过**的草稿提议整体替换（`DraftReviewBody` 是整体替换，不是合并） | 只在正文为空或只有程序数字块时 / 任何草稿都可以（CAS 防并发覆盖，但人会在提议里看到自己的字被整体换掉） | 只在正文为空或只有程序数字块时（M6） |
| Q8 | classify 模型 `confidence = low` 时要不要仍给提议 | 不给 / 给（`rationale` 标注低置信）/ 按阈值 | 不给。**评审给的技术输入**：小模型自报置信度的校准很差，`low/medium/high` 未必对应真实命中率；阈值应当用 J26 冒烟里各档的实际命中率来定，而不是先验拍一个 |

### 11.12 附录：scratch crate 与 `cargo` 记录

路径：`/private/tmp/claude-502/-Users-jason-Dev-auraai-Agent24/977deb42-1aba-448f-95e7-5bae2dee6fd4/scratchpad/t501-check/`（`sin90 = { path = "<本 worktree>" }`，另依赖 `serde`/`serde_json`/`sha2`/`hex`/`thiserror`/`tokio`/`syn`/`proc-macro2`/`sqlx`；target 在 crate 自己目录下）。模块：`ops.rs`（§11.2 wire + F-1）、`ctx.rs`（§11.2.3 校验矩阵与叠加）、`ports.rs`（§11.3/§11.5 端口、梯、三种动作、熔断、预算、绊线、来源推导、批量 `precheck`）、`classify.rs`、`summarize.rs`（规范化、原子单元、标题清理、第 2 轮全部绕过的负对照）、`boundary.rs`（syn 白名单，含 `self` 剥离、`STD_DENY`、`cfg(test)` 跳过）、`dryrun.rs`（SAVEPOINT 试跑）、`readonly.rs`（`query_only` 读取器在文件 WAL 与命名共享缓存下的行为）。v2.1 改动的对外签名：`is_program_only(body, current_facts_md)` 由一参变两参、`AiSink::precheck` 改为批量——评审的 `review-probe`（v1 签名）与 `review2-probe` 中用到这两处的探针需要随之更新。

```
$ cargo clippy --all-targets   # 0 warning
$ cargo test
test classify::tests::normalize ... ok
test ctx::tests::overlay_second_assign_of_same_task_rejected ... ok
test ctx::tests::assign_positive_and_negative ... ok
test ctx::tests::overlay_sees_an_earlier_close_in_the_batch ... ok
test classify::tests::parse_accepts_known_key_and_rejects_invented_one ... ok
test classify::tests::schema_enum_lists_keys_plus_none ... ok
test boundary::tests::cfg_test_items_are_skipped_exactly ... ok
test ctx::tests::draft_body_cas_chain_and_rejections ... ok
test ops::tests::existing_sin90op_silently_accepts_unknown_fields ... ok
test ops::tests::new_ops_reject_unknown_fields ... ok
test boundary::tests::clean_source_passes ... ok
test ops::tests::sha_of_empty_body_is_the_well_known_constant ... ok
test ops::tests::wire_shape_round_trips ... ok
test ports::tests::bad_output_degrades ... ok
test ports::tests::busy_defers_item_and_rest_of_run_without_r2 ... ok
test boundary::tests::positive_controls_each_trip ... ok
test ports::tests::circuit_breaker_skips_engine_for_rest_of_run ... ok
test ports::tests::call_budget_defers ... ok
test ports::tests::connection_lost_and_generation_ending_abort ... ok
test ports::tests::failure_table_is_exhaustive ... ok
test ports::tests::local_unavailable_degrades_to_reflex_and_row_is_atomic_with_proposal ... ok
test ports::tests::executive_request_served_locally_is_local_brain ... ok
test ports::tests::plan_shapes ... ok
test ports::tests::remote_down_local_up_is_local_brain ... ok
test ports::tests::switch_turned_off_mid_run_trips_remote_reply ... ok
test ports::tests::run_item_future_is_send ... ok
test summarize::tests::a_value_never_appears_without_its_own_label ... ok
test summarize::tests::facts_come_from_the_draft_verbatim ... ok
test summarize::tests::every_digit_in_body_comes_from_facts_or_titles ... ok
test summarize::tests::markdown_and_html_flattened ... ok
test summarize::tests::known_false_positives_are_what_m3_says ... ok
test summarize::tests::program_only_is_exact_match_with_current_block ... ok
test summarize::tests::titles_are_sanitized_when_substituted ... ok
test summarize::tests::round2_bypasses_are_rejected ... ok
test readonly::tests::memory_shared_cache_query_only ... ok
test dryrun::tests::valid_reorder_inserts_pending_but_dry_run_leaves_no_trace ... ok
test dryrun::tests::invalid_reorder_rejected_at_submit_and_nothing_written ... ok
test readonly::tests::file_wal_query_only ... ok
test readonly::tests::memory_reader_waits_for_other_tasks_dirty_tx_then_succeeds ... ok
test result: ok. 39 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.21s
```
