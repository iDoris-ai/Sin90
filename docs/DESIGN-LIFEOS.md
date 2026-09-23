# Sin90 Life OS —— 正式设计文档 v1

> 文档类型：设计裁决（Design Decisions）+ 里程碑拆解
> 输入：[`LIFEOS-DESIGN-INPUT.md`](LIFEOS-DESIGN-INPUT.md)（2026-09-20 用户构想 + GPT 提议）
> 核对对象：Agent24 主仓库 `rust/crates/agent24-sin90{,-store,-os}`（2026-09-20 实读源码，非文档转述）
> 状态：本文冻结数据模型与 M0 范围；M1+ 只给方向，不冻结
> 最后更新：2026-09-24（T5.0.1 追加 §11 AI v1 设计补丁，草稿 v1 待评审；§1.3 / §2 #17–#24 / §3.3 / §4.1 / §6 M5 随之更新）

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
| 20 | `sin90_ai_calls` 加列 `run_id / proposal_id / served_tier / model_id / error_kind` | **采纳** | 原表（`0001_sin90.sql:120-128`）回答不了验收要的两件事：这条提议是哪次调用产出的（`proposal_id`）、调用实际在本地还是远端服务（`served_tier`——内核按 `complexity` 路由，模块请求的引擎 ≠ 实际服务层级，Agent24 ME4-S2 §2.2/§4.3）。纯 `ADD COLUMN`，全部可空。§11.6。 |
| 21 | 新表 `sin90_settings(key PK, value, updated_at)`，首个键 `ai.executive_enabled` | **采纳** | executive 需要「用户设置开启」，设置必须持久、只由人改、改动留痕。单独一张 KV 表而不是塞进 `actor-keys.json`（那是凭据文件，权限 0600，语义不同）。改动走人类直写路由 + `setting.changed` 事件（#12 的硬约束）。缺行 = 关。 |
| 22 | classify 允许「只指派 Area、不指派 Direction」（给 `sin90_tasks` 加 `area_id`） | **拒绝** | §3.1 的对象图里 Area 在 Direction 之上、不与 Task 并列；`list_tasks` 按 Area 过滤就是经 `sin90_directions` join（`repo.rs:1214-1240`）。加列 = Task 有两条可能互相矛盾的归属路径。classify 在「有 Area 没有合适 Direction」时**不产出提议**（任务留在 inbox，理由写进 run 结果）。**复审触发条件**：用户反馈 inbox 里长期堆着「知道属于哪个领域、但不值得为它开方向」的条目。 |
| 23 | reflex（纯规则）产出的提议 `source = rule` | **改造（改 §M5 验收原文）** | §M5 原文只列 `{local_brain, executive}`，但引擎梯最底层 reflex 不调任何模型；把它记成 `local_brain` 是「措辞比机制强」（§2.1）。`rule` 早在 `ProposalSource` 里（`proposal.rs:89-95`）。T5.5.1 的 SQL 本来就含 `rule`。 |
| 24 | M5 用内核私有记忆（T4.4.1 写入的定稿摘要）做 AI 上下文 | **v1 不用** | SQLite 是真相（architecture.md 边界 #1），summarize 需要的「上周定稿」本地直接可读；走 `_a24/memory/private/recall` 只多一个失败面，且 v1 三个能力没有一个需要跨设备/跨模块上下文。T4.4.1 照做（它的价值是给内核侧 agent 用）。**复审触发条件**：出现需要「非 Sin90 数据」的 AI 能力。 |

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
| `sin90_ai_calls`（T5.1.1，§11.6） | + `run_id TEXT NULL`、`proposal_id TEXT NULL`、`served_tier TEXT NULL`（`local\|remote`，reflex/失败为 `NULL`）、`model_id TEXT NULL`、`error_kind TEXT NULL`；+ `idx_sin90_ai_calls_run(run_id)`、`idx_sin90_ai_calls_proposal(proposal_id)`。`task_kind` 的值域定为 `classify\|summarize\|propose`，`engine` 仍是 `reflex\|local\|executive`（请求的引擎）。迁移取当时的 max+1（spec.md「迁移编号不预分配」），与下一行同一个文件 | 加列 |
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
**T5.0.1 设计补丁（§11，草稿 v1，待评审）对这段的两处收紧/改动**：
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

> **草稿 v1，待评审**（2026-09-24）。评审方：Codex 额度 2026-09-29 19:28 前耗尽，期间按 tasks.md T5.0.1 由**全新上下文的 Opus 子代理**做对抗评审（Critical/High/Medium/Low + file:line），评审记录逐轮追加在本节头部的表里，到 APPROVE 才改为「冻结」。
>
> | 轮次 | 评审方 | 结论 | C / H / M / L |
> |---|---|---|---|
> | —— | —— | 待评审 | —— |
>
> 事实核对基线：本 worktree `docs/t5.0.1-ai-v1-design`（叠在 `feat/t4.2.1-body-ref` `7925c4d` 上）；类型化客户端在分支 `feat/t3.2.1-kernel-clients`（`db80b88`）；内核推理回调以 Agent24 `docs/design/ME4-S2-model-callback.md` **v3.1 冻结版**为准（本文引用不复述，§10）。
> 文中每一段 Rust 签名都在 scratch crate `t501-check`（path 依赖本 worktree 的 `sin90`）里 `cargo check --all-targets` + `cargo test` 过，见 §11.12。
> 标 ⚖️ 的数值是**选的**，不是推出来的。标 🟡 的是**待用户拍板的产品问题**（§11.11），文中给的是拍板前的**最保守占位**，不是结论。

### 11.0 解决什么、不解决什么

**解决**（tasks.md T5.0.1 目标 + T5.1.1–T5.5.1 的验收都要在这里有落点）：

| 问题 | 位置 | 一句话结论 |
|---|---|---|
| classify 的产出落成什么变更 | §11.2.1、§2 #17/#22 | 新 Op `AssignTaskDirection{task_id, direction_id}`，只作用于 inbox；Area 由 Direction 派生，不单独指派 |
| summarize 的产出落成什么变更 | §11.2.2、§2 #18 | 新 Op `DraftReviewBody{review_id, base_body_sha256, body}`，只作用于草稿，正文摘要做比较并交换 |
| 校验放哪、与 `Working` 叠加怎么交互 | §11.2.3、§2 #19 | `ValidationCtx` 加三个读法；`Working` 加两张叠加表；apply 里补关系约束 |
| 引擎梯 | §11.3 | classify：reflex(决定性) → [executive] → local → reflex(兜底)；summarize/propose：[executive] → local → reflex。失败**按错误种类**降级或中止，每次尝试一行 `sin90_ai_calls` |
| executive 的开关在哪、默认什么 | §11.3.2、§2 #21 | manifest `model_access: remote_allowed` **且** `sin90_settings['ai.executive_enabled'] = true`；🟡 占位：manifest 保持 `local_only`、开关默认关 |
| 三个能力各自的输入/输出/规则/提示词/触发/限流 | §11.4 | 三条 `POST /ai/*` 触发路由，后台 run、单飞、批量上限；模型输出一律 `response_format: json_schema, strict` + 程序复核 |
| summarize「数字只来自草稿」怎么保证 | §11.4.2 | 数字区由程序从 T4.3.1 草稿渲染；模型只写叙述，叙述里的字面数字一律拒，数值只能用 `{{fN}}` 占位由程序代入 |
| AI 模块碰不到直写接口 | §11.5 | `ai/` 只依赖三个 trait（只读模型、两个写法的 sink、模型端口）；文本结构测试 + 行为级表快照测试 |
| 判据 | §11.7 | J1–J26，覆盖 T5.1.1–T5.5.1，每条带正对照 |

**不解决**（写进 §11.9 残余风险或留给后续）：
- 提议的**拒绝**路由（今天没有 `POST /proposals/{id}/reject`，过期提议只能一直 `pending`）——🟡 Q6。
- 改**已归类**任务的归属、跨 Area 迁移 Direction、AI 建 Direction/Area（§2 #17/#22 的复审触发条件）。
- daily / rhythm 复盘的 summarize（T4.3.1 只给周草稿；没有数字来源就没有「数字只来自草稿」可言）。
- 按日 token/费用预算（内核 ME4-S2 §0 明确不做；Sin90 只有次数与并发上限）。
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

### 11.2 新 Op（§2 #17–#19）

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

**对重放方的影响（交给 T4.3.1）**：任务的 Direction 归属不再只在 `task.created` 的 payload 里——按 Direction/Area 统计「完成任务数」的回放必须把 `task.direction_assigned` 折叠进来（取完成事件之前最后一次归属）。attention 回放按 `ScheduleBlock.direction_id` 计时，不受影响。

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

| 引擎 | 是什么 | `ProposalSource` | 可用条件 |
|---|---|---|---|
| `reflex` | 纯规则，进程内，无 I/O 之外的依赖（只读 Sin90 自己的库） | `rule` | 永远可用（含 standalone、内核未授予 models、断网） |
| `local` | `_a24/model/complete`，`complexity: simple` | 按 `result.tier`：`local` → `local_brain` | 握手 `Offer.provides` 含 `_a24/model/`（`ModelClient::new` 返回 `Some`） |
| `executive` | `_a24/model/complete`，`complexity: complex` | 按 `result.tier`：`remote` → `executive`，`local` → `local_brain` | 上一行 **且** 编译期 manifest `model_access == remote_allowed` **且** `sin90_settings['ai.executive_enabled'] == true` |

**关键事实（决定了本节的措辞）**：Sin90 **选不了** provider（ME4-S2 §2.2）。「executive」只是「请内核按 `complex` 路由」，内核可能仍用本地服务它；所以 `source` 按 **`result.tier`（实际服务层级）** 定，不按请求的引擎（scratch `executive_request_served_locally_is_local_brain`）。

#### 11.3.2 executive 的两道闸与它们各自保证什么

1. **manifest `model_access`**（内核强制）：Sin90 的 `domain-os.yml` 是否声明 `remote_allowed`。Sin90 用 `include_str!("../domain-os.yml")` 在编译期取这个值（`const MODEL_ACCESS`），只用来决定**要不要尝试** executive——真正的隐私保证在内核。
2. **用户设置**（Sin90 自己的）：`sin90_settings` 表的 `ai.executive_enabled`，缺行 = `false`；只能经 `PUT /settings/ai {"executive_enabled": bool}` 改（`require_human`，`deny_unknown_fields`，同事务写事件 `setting.changed` payload `{key, value}`），`GET /settings/ai` 读（任一 key）。

**必须如实写出的一条（措辞不能比机制强）**：一旦 manifest 是 `remote_allowed`，内核对 Sin90 的**所有**调用都以 `Privacy::Any` 路由——包括 `local` 引擎发的 `simple` 调用：本地 provider 不可用时，内核会**自己**把它送到远端（ME4-S2 §2.2「`Simple` 本地优先」= 本地不行就远端）。模块没有逐次调用收窄隐私的字段（ME4-S2 §4.2：`privacy` 字段不存在）。所以：
- manifest `local_only`：`local` 调用**由内核保证**不出本机（以 ME4-S2 §2.3 的 `Local` 定义为准）；executive 永不尝试。
- manifest `remote_allowed` + 用户开关**关**：Sin90 不发 `complex` 调用，但**不能保证** `simple` 调用不被内核送到远端；Sin90 只能**事后发现**（`result.tier == remote` 且开关关 → 丢弃结果、记 `error_kind = privacy_tripwire`、降级，scratch `remote_reply_while_executive_off_trips_and_degrades`）——字节已经出去了，这是检测不是防护。
- 🟡 **占位**：发布包保持 `local_only`（不写 `model_access`），executive 代码路径存在、被测试覆盖（测试包用 `remote_allowed` 的 manifest 变体），但**正式包里不可达**；用户开关默认关。是否发布 `remote_allowed` 版本、是否向 Agent24 提「逐次收窄到 LocalOnly」的字段（只能收窄、不能放宽，不破坏 ME4-S2 的隐私模型），见 §11.11 Q1/Q2。

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

- **向上（升级）**：只有 classify 有 `ReflexDecisive`——规则能**确定**时不花模型（§11.4.1 R1）；确定不了是正常情况，不叫失败，`fallback_from` 不记。
- **向下（降级）**：模型尝试失败按下表处理；降到下一步时，下一步那行的 `fallback_from` = 刚失败的引擎。
- **同一引擎不重试**（v1）：本地推理基本串行、内核满了就 `busy` 不排队，同一 run 里原地重试只会再撞一次；用户可以再触发一次 run。

#### 11.3.4 失败 → 动作（`ModelFailure::action()`，scratch 已 check + test）

`ai/` 不认识 `ClientError`（它不许 import `adapter_agent24`，§11.5）；adapter 实现 `ModelPort` 时把 `ClientError` 折叠成 `ModelFailure`：

| `ClientError`（T5.1.1 补齐后） | `ModelFailure` | 动作 | `error_kind` |
|---|---|---|---|
| `Unavailable{retryable:true, cause:no_provider}` | `Unavailable{..}` | **降级** | `unavailable.no_provider` |
| `Unavailable{retryable:false, cause:request_rejected}` | 同上 | **降级**（本请求对这个模型不合格；reflex 不受影响） | `unavailable.request_rejected` |
| `Unavailable{retryable:false, cause:backend_config}` | 同上 | **降级** + `warn!`（后端配置坏了，要告诉用户，但不该让 inbox 分类停摆） | `unavailable.backend_config` |
| `Unavailable{retryable:false, cause:response_too_large}` | 同上 | **降级**（我们的 `max_tokens` ≤ 1024，出现即本端 bug，`error!`） | `unavailable.response_too_large` |
| `Busy` / `RateLimited` | `Busy` / `RateLimited` | **降级**（不排队、不原地退避：交互式触发，等不起） | `busy` / `rate_limited` |
| `Timeout`（本端 125s 或内核 120s） | `Timeout` | **降级** | `timeout` |
| `Forbidden`（未授予 models） | `Forbidden` | **降级**（正常构造时 port 就是 `None`，走不到这里；到了说明 offer 与授予不一致，`warn!`） | `forbidden` |
| `InvalidParams` / `PayloadTooLarge` | `BadRequest` | **降级** + `error!`（本端 bug） | `bad_request` |
| `NotReady` / `Draining` / `Revoked` / `NotSent` / `RequestNotInFlight` / `NotFound` / `QuotaExceeded` / `TokenInvalid` / `Other` | `NotUsable` | **降级**（reflex 不需要内核） | `not_usable` |
| `ConnectionLost` | `ConnectionLost` | **中止整个 run**，不走 reflex、不产提议（结果不确定、这一代正在结束，architecture.md 边界 #5；进程随后退出） | `connection_lost` |
| `Cancelled`（内核停机） | `Cancelled` | **中止** | `cancelled` |
| 模型返回了，但输出不合格（JSON 解析失败、不在 schema 的 enum、叙述里有数字……§11.4） | —— | **降级** | `bad_output` / 各能力自己的子种类 |
| 返回了，`tier == remote` 而开关关 | —— | **降级**（丢弃结果） | `privacy_tripwire` |

T5.1.1 对 `ClientError` 的配套改动（在 adapter 里，交给实现）：加 `Unavailable{retryable: bool, cause: UnavailableCause}` 与 `Cancelled` 两个变体；`map_rpc_error` 认 `unavailable`（读 `data.retryable`、`data.cause`，cause 不在四值闭集内 → `Other`）与 `cancelled`；`every_spec_error_kind_maps_to_its_documented_variant` 的计数 17 → 18（`unavailable`）并把 `cancelled` 从「落 `Other`」移出；`is_permanent`：`Unavailable{retryable:false}` 为真；`is_retryable`：`Unavailable{retryable:true}` 为真。

#### 11.3.5 `sin90_ai_calls`：每次尝试一行

**每个 `Step` 实际执行一次写一行**（跳过的步——比如 port 不在时的 `Model(Local)`——不写）。scratch `AiCallRecord`：

| 列 | 值 |
|---|---|
| `id` | ULID |
| `run_id`（新） | 本次 `POST /ai/*` 的 run id；classify 的一个 run 覆盖多个条目 |
| `task_kind` | `classify` / `summarize` / `propose` |
| `engine` | **请求的**引擎 `reflex` / `local` / `executive` |
| `fallback_from` | 上一步失败的引擎；升级（`ReflexDecisive` 无结论 → 模型）不记 |
| `served_tier`（新） | 模型返回时的 `result.tier`；reflex 与传输失败为 `NULL` |
| `model_id`（新） | `result.model_id`（内核给 `null` 就 `NULL`） |
| `latency_ms` | 本步墙钟（含内核排队与推理） |
| `ok` | 本步**产出了可用结果**（模型返回且输出通过复核 / reflex 有结论） |
| `error_kind`（新） | §11.3.4 的串；`ok = 1` 时为 `NULL`；reflex 无结论为 `no_match` |
| `proposal_id`（新） | 只在**产出提议的那一行**填；每条 AI 提议恰好对应一行 `ok = 1` 的调用记录 |
| `at` | ISO-8601 |

写入走 `AiSink::record_call`，失败只 `warn!` 不阻断 run（调用记录是审计不是业务真相；丢一行不该让分类失败）——这意味着「可追溯」是**尽力而为**，§11.9 R6。

### 11.4 三个能力的契约

**公共部分**：
- **触发**：三条路由，全部 `require_any_actor`（触发本身只写 `sin90_proposals` 的 `pending` 行与 `sin90_ai_calls`，不改业务状态；人类 UI 与 Pet0 这类自动化都可以点「帮我分类」），返回 `202 {"run_id", "capability"}`，run 在后台跑：
  `POST /ai/classify {"task_ids"?: [...]}`、`POST /ai/summarize {"review_id"}`、`POST /ai/propose {"week_id"}`；`GET /ai/runs/{run_id}` 返回 `{run_id, capability, state: running|done|aborted, proposals: [...], calls: [...sin90_ai_calls 行]}`（运行态在进程内存里，上限 64 条 LRU ⚖️；重启后只剩 `calls`，`state` 由「有没有行」推不出来就报 `unknown`）。
  standalone 模式同样注册（port 为 `None`，只有 reflex）。**不在** `/_a24/*` 下，不依赖内核代理。
- **为什么后台跑、不绑 `request_id`**：被代理请求的总时限是 30s（§11.1 第 12 条），一次本地推理可以到 120s；绑上 `request_id` 就会在 30s 被内核截断（`RequestNotInFlight`）。所以模型调用**不带 `request_id`**，run 的生命周期属于 Sin90 进程；进程退出时在途 run 直接丢弃（已写的调用记录与已提交的提议保留）。
- **限流（Sin90 这一侧）**：每个能力**单飞**——同一能力已有 run 在跑，再触发返回 `409 {"code":"ai_busy","run_id":<在跑的>}`；进程内模型调用信号量 = 2（= 内核每模块在途上限，本端不自己制造 `busy`）；本端对 `_a24/model/complete` 用 `call_with_timeout(125s)` ⚖️（内核 120s + 5s 余量，§11.1 第 11 条）。内核的令牌桶（30 突发 / 0.5 每秒）是最终上限。
- **自动触发**：🟡 占位 = **v1 只有手动触发**（没有 Routine fired / capture 后自动跑）。见 Q3。
- **提议的形状**：`id = "ai-<capability>-<ULID>"`（只是便于人看；「这是不是 AI 产出」以 `sin90_ai_calls.proposal_id` 关联为准，J24）；`status = pending`；`source` 按 §11.3.1；`rationale` = `"<engine>: <理由>"`，去控制字符、截到 280 字符 ⚖️。
- **提交前校验**：AI 经 `AiSink::submit` 提交，它在**一个** `BEGIN IMMEDIATE` 里 `build_snapshot` → `validate` → `INSERT … 'pending'`；校验不过就不入库（`SinkError::Invalid`，该条目计入 run 结果的 `rejected`）。accept 时照旧再校验一次（状态可能已变）。人类/自动化 key 的 `POST /proposals` 不变（F-2）。
- **每个能力只准产出自己的 Op**：`AiSink::submit` 的 store 实现先查 `allowed_ops(capability)`——classify ⇒ 仅 `AssignTaskDirection`；summarize ⇒ 仅 `DraftReviewBody`；propose ⇒ 仅 `CarryOverTask` / `ReorderTasks` / `CreateTasks`。越界即 `SinkError::Invalid`。这让「只有三个能力」在写入口有一道机制，而不只是代码约定（J22）。
- **去重**：触发时跳过已有**仍然有效**的挂起提议的目标（classify：仍在 inbox 的任务；summarize：base 摘要仍等于当前正文的复盘）。已过期的挂起提议（任务已被归类、正文已被改）不挡新 run——否则没有拒绝路由（Q6）时，一条过期提议会永久挡住这个目标。

#### 11.4.1 classify（T5.2.1）

- **输入**：`task_ids` 给了就用（每个必须在 inbox，否则 400）；没给就取 inbox 里最老的、未被去重挡掉的条目。每 run 至多 **20** 条 ⚖️（20 次调用在内核 30 次突发之内）。
- **候选集**：非终态 Direction，按 `updated_at` 倒序至多 **40** 个 ⚖️（`{direction_id, title, status, area_title}`）；候选为空 → 该条目直接结束（run 结果记 `no_candidates`，不写调用行、不产提议）。
- **reflex R1（决定性）**：`normalize_title`（去首尾空白、压缩内部空白、小写，scratch 已测）后，库里**已归类**（非 inbox）、归属 Direction 仍非终态的任务中，同标准化标题的任务**全部**指向同一个 Direction D → 提议 `AssignTaskDirection(t, D)`，`source = rule`，不调模型。
- **模型（executive / local）**：
  - 候选以**不透明短键** `d1…dn` 呈现（scratch `candidate_keys`），模型看不到 ULID，也就造不出 ULID。
  - messages：`system` = 固定指令（「从候选里选一个最合适的方向；都不合适就选 none；只输出 JSON」）；`user` = JSON `{"item": {"title": …}, "candidates": [{"key":"d1","title":…,"area":…}, …]}`。
  - `response_format`：`{"type":"json_schema","json_schema":{"name":"sin90_classify","strict":true,"schema": <scratch classify::schema>}}`——`{choice: enum[d1…dn, none], confidence: enum[low, medium, high], reason: string ≤ 200}`，`additionalProperties: false`。`max_tokens: 256` ⚖️。
  - 程序复核（scratch `classify::parse`）：容忍一层 ```` ```json ```` 围栏；`deny_unknown_fields`；`choice` 必须在本次的键集内（否则 `bad_output`）；`none` 或 `low` → **本条不产提议**（记 `ok = 1`，不是失败——模型认真地说了「不知道」）🟡 Q8。
- **reflex R2（兜底，仅在模型步全部失败或不存在时）**：任务标题与「候选 Direction 标题 + Area 标题」的重合度——ASCII 按长度 ≥ 3 的词、CJK 按字二元组；**唯一最高分**且 ≥ 2 个二元组或 ≥ 1 个词 ⚖️ → 提议；否则 `no_match`、不产提议。R2 是弱规则，它的存在只是让「模型全挂」时不至于完全没有建议；它产出的提议 `source = rule`，人一眼能看出不是模型给的。
- **输出**：每个条目至多一条提议，`ops = [AssignTaskDirection]`。条目之间独立（人可以只批其中几条）。

#### 11.4.2 summarize（T5.3.1）

- **输入**：`review_id`，必须 `kind = weekly` 且 `status = draft`（否则 409；daily/rhythm 400 `unsupported_kind`）；周 = `period`。
- **数字来源**：`AiReadModel` 调 T4.3.1 的周草稿函数得到 `WeeklyDraft`（形状见 §11.1 第 13 条）；另取该周 `task.transitioned → done` 事件对应的任务标题（至多 50 条 ⚖️，只作叙述素材，不作数字来源）。
- **「数字只能来自草稿」的机制**（scratch `summarize.rs` 已 check + test）：
  1. `facts(draft, title_of)` 把草稿的每个数值变成一条 `Fact{key: "fN", label, value}`；`render_facts` 把它们渲染成正文的「本周数字」块——**这一块完全由程序生成**，模型碰不到。
  2. 模型只输出 `{"narrative": string ≤ 2000}`（`response_format` 同上，`max_tokens: 1024` ⚖️），提示词给它 `[{key, label, value}]` 并要求：**不许写任何数字**，要提到数值就写 `{{fN}}`，要提到标签就写 `{{fN.label}}`。
  3. `fill_narrative` 复核：占位符之外的字面文本里出现任何 `char::is_numeric()` 为真的字符（阿拉伯、全角、其它 Unicode 数字）→ 拒；出现「汉字数词串 + 量词」（`七个`、`两小时`、`三次`……）→ 拒；未知占位符、未闭合 → 拒；拒 = `bad_output`，降级。通过后程序把占位符代成 `Fact` 的值/标签。
  4. 正文 = `compose_body(facts 块, Some(叙述))`；reflex（兜底）= `compose_body(facts 块, None)`，只有数字块、没有叙述。
  结论的**准确说法**：正文里「本周数字」块的每个数字逐字来自草稿；叙述块里的数值只能以占位符形式出现、由程序代入同一批值；叙述块不含阿拉伯/全角数字与「汉字数词 + 量词」。**不**声称模型无法表达数量（「近半」「翻倍」「seven」这类仍能漏过，§11.9 R1）。
- **比较并交换**：提议的 `base_body_sha256` = 触发时读到的正文摘要；正文与新正文相同 → 不产提议（`NoChange`，run 结果记 `unchanged`）。
- **数字的时效**：数字冻结在提议生成时；accept 前又有 block 完成，提议里的数字就旧了。人重新触发一次即可；v1 不做 accept 时重算（那等于让 apply 调模型）。§11.9 R4。
- **非空草稿**：🟡 占位 = 允许（人写过的草稿也能收到 AI 改写提议，CAS 保证不会覆盖提议之后的编辑）。Q7。

#### 11.4.3 propose（T5.4.1）

- **输入**：`week_id` = 目标周 W，必须 open（planning/active），否则 409。
- **读**：W 的非终态任务；**上一周** P = `iso_week` 小于 W 的最近一周，且仍 open（reviewing/closed 的周里的任务不许动，`require_task_week_open`）——P 不存在或已关就没有顺延建议；Rhythm 当前配额（非 retired 的最新一条）；W 里没有任何任务、但配额 pct > 0 的 Direction = 「缺口 Direction」。
- **只用三个既有 Op**，分成**至多三条独立提议**（人可以分开批）：
  1. `propose.carry`：`[CarryOverTask(t, W) …]`，t ∈ P 中 planned/in_progress 的任务（`backlog → carried_over` 不合法，`transitions.rs:121-133`）。
  2. `propose.reorder`：`[ReorderTasks{week_id: W, order}]`，`order` 是 W 全部非终态任务的一个**排列**（程序保证：模型漏掉的按原顺序补在后面，重复即 `bad_output`）；与当前 `sort_key` 顺序相同 → 不产。
  3. `propose.create`：`[CreateTasks{week_id: W, tasks: [{title, direction_id}]}]`，至多 3 条 ⚖️，`direction_id` 只能是缺口 Direction；**只有模型步产出**（reflex 不编标题）。
- **reflex**：carry = P 中全部 planned/in_progress；reorder = in_progress → planned → backlog，同层按所属 Direction 的配额 pct 降序、再按 `created_at`；create = 无。
- **模型**：候选任务与缺口 Direction 同样用不透明键（`p1…` / `w1…` / `g1…`）；schema `{carry: [enum p*], order: [enum w*], new_tasks: [{title: string 1..120, direction: enum g*}] ≤ 3, reason: string ≤ 200}`，`max_tokens: 512` ⚖️；复核：键必须在集合内、`order` 无重复、标题去控制字符后非空。
- **半应用不可能**：每条提议 accept 是一个事务；`ReorderTasks` 引用不在该周的任务 → apply 内 `NotFound` 整体回滚（§11.1 第 4 条）；`CarryOverTask` 与 `ReorderTasks` 不在同一条提议里，所以「顺延产生的新任务 id 无法在同批排序」不成问题。**已知的小瑕疵**：先批 reorder、再批 carry，顺延进来的任务 `sort_key = 0`（`repo.rs` `CarryOverTask` 分支），会与排第一的任务并列（§11.9 R9）。

### 11.5 结构约束：AI 模块只能提议（T5.1.1）

**端口**（scratch `ports.rs` 已 check + test，含 `tokio::spawn` 证明 run 的 future 是 `Send`）：

```rust
pub trait ModelPort: Send + Sync {
    fn complete(&self, req: ModelRequest) -> impl Future<Output = Result<ModelReply, ModelFailure>> + Send;
}
/// ai/ 的全部写能力：只有这两个方法。由 Sin90Store 在 store/ai_port.rs 实现。
pub trait AiSink: Send + Sync {
    /// 一个 BEGIN IMMEDIATE：allowed_ops(cap) → build_snapshot → validate → INSERT 'pending'。永不 apply。
    fn submit(&self, cap: Capability, p: Sin90Proposal) -> impl Future<Output = Result<(), SinkError>> + Send;
    fn record_call(&self, rec: AiCallRecord) -> impl Future<Output = Result<(), SinkError>> + Send;
}
pub trait AiReadModel: Send + Sync {
    fn settings(&self) -> impl Future<Output = Result<AiSettings, ReadError>> + Send;
    fn inbox(&self, limit: u32) -> impl Future<Output = Result<Vec<Task>, ReadError>> + Send;
    fn direction_candidates(&self, limit: u32) -> impl Future<Output = Result<Vec<DirectionCandidate>, ReadError>> + Send;
    fn title_history(&self, normalized: &str) -> impl Future<Output = Result<Vec<DirectionId>, ReadError>> + Send;
    fn pending_targets(&self, cap: Capability) -> impl Future<Output = Result<Vec<String>, ReadError>> + Send;
    fn review(&self, id: &str) -> impl Future<Output = Result<Option<Review>, ReadError>> + Send;
    fn week_tasks(&self, week_id: &WeekId) -> impl Future<Output = Result<Vec<Task>, ReadError>> + Send;
    // T5.3.1 / T5.4.1 各自再加：weekly_draft(week)、done_titles(week)、previous_open_week(week)、rhythm_alloc()
}
```

**依赖方向**：`ai/` 只 `use crate::core::*` 与 `crate::ai::*`；`store/ai_port.rs` 实现 `AiReadModel + AiSink`（store → `ai::ports`，后者只含 trait 与值类型）；`adapter_agent24/clients/model.rs` 实现 `ModelPort`；`http/` 组装三者、起后台 run。`ai/` 里的函数全部对三个 trait 泛型，**不出现具体类型**。

**三层判据，从弱到强**：
1. **文本结构测试** `ai_boundary`（J7）：遍历 `src/ai/**/*.rs`，对每个文件 `forbidden_refs(src)` 必须为空。检查器先剥 `//` 注释、再去掉**全部空白**后找子串：`crate::store`、`::store::`、`super::store`、`Sin90Store`、`sqlx`、`crate::http`、`super::http`、`adapter_agent24`、`#[path`、`include!`（scratch `boundary.rs`，正对照覆盖 `crate :: store`、`super::super::store::Sin90Store`、`#[path = …]`、`include!(…)` 等写法）。这是 T5.1.1 要的「编译期/grep 测试」；它挡的是**顺手 import**，挡不住刻意绕路（R2）。
2. **类型层**：`ai/` 的一切写都只能经 `AiSink` 的两个方法；`AiSink` 的 store 实现只写 `sin90_proposals`（`pending`）、它自带的 `proposal.submitted` 事件、`sin90_ai_calls`。
3. **行为级表快照**（J8，最强）：在夹具库上用桩模型把三个能力各跑一遍，比较运行前后**除** `sin90_proposals`、`sin90_ai_calls`、`sin90_events WHERE entity = 'proposal'` 之外所有表的全部行——必须逐字节相同。正对照：随后用人类 key accept 其中一条 → 快照必变。这条不关心代码怎么写，只关心库里发生了什么，文本检查器漏掉的绕路它也会抓到。

**为什么不拆 crate**：把 `core` 拆成独立 crate、`ai` 另成 crate 只依赖 `core`，才是真正的编译期保证；但今天 `core/store/http` 是一个 crate，拆分是一次与 M5 无关的大改（TS.1.1 换 SDK 时一起评估更合适）。v1 以「文本检查 + 行为快照」组合代替，并如实写成「结构测试」而不是「编译期保证」。

### 11.6 存储改动（§2 #20/#21，§4.1 已登记）

一个迁移文件（编号取当时 max+1，spec.md「不预分配」）：

```sql
ALTER TABLE sin90_ai_calls ADD COLUMN run_id      TEXT;
ALTER TABLE sin90_ai_calls ADD COLUMN proposal_id TEXT;   -- 不加 FK：调用记录先于提议写入，且提议提交可能失败
ALTER TABLE sin90_ai_calls ADD COLUMN served_tier TEXT;   -- local|remote|NULL，无 CHECK（与 outbox.status 同一约定：代码约束）
ALTER TABLE sin90_ai_calls ADD COLUMN model_id    TEXT;
ALTER TABLE sin90_ai_calls ADD COLUMN error_kind  TEXT;
CREATE INDEX idx_sin90_ai_calls_run      ON sin90_ai_calls(run_id);
CREATE INDEX idx_sin90_ai_calls_proposal ON sin90_ai_calls(proposal_id);
CREATE TABLE sin90_settings (
    key        TEXT PRIMARY KEY,
    value      TEXT NOT NULL,          -- JSON 标量
    updated_at TEXT NOT NULL
);
```

**写入顺序**：产出提议的那一步先 `submit`、成功后再 `record_call(ok=1, proposal_id)`；`submit` 失败则 `record_call(ok=0, error_kind="rejected_by_validate")` 并继续下一条目（不降级——模型没错，是状态变了）。事件：`setting.changed`（`entity = setting`、`entity_id = <key>`）是本补丁唯一新增的非 Op 事件。

### 11.7 判据（每条带正对照；`cargo test <过滤>` 先 `-- --list` 断言匹配数 > 0；新回归测试一律变异验证）

**T5.1.1 引擎梯 + 调用记录**

| # | 测试（过滤名） | 断言 | 正对照 / 变异 |
|---|---|---|---|
| J1 | `ai_ladder_local_unavailable_degrades_to_reflex` | classify 一条、R1 无结论、local 桩回 `unavailable/no_provider`、R2 有结论 → 恰好 3 行调用记录：`reflex ok=0 no_match`、`local ok=0 unavailable.no_provider`、`reflex ok=1 fallback_from=local proposal_id=<p>`；提议 `source = rule` | 桩改回成功 → 2 行、`source = local_brain`、没有 `fallback_from` |
| J2 | `ai_ladder_connection_lost_aborts` | `ConnectionLost` → run `aborted`、0 条提议、之后**没有** reflex 行 | 同一位置换 `Timeout` → 降级并产出 reflex 提议 |
| J3 | `ai_ladder_failure_table` | 对 `ModelFailure` 每个变体断言 `action()` 与 §11.3.4 一致；测试里用**穷尽 `match`**（新增变体不改测试就编译失败） | 变异：把 `ConnectionLost` 改成 `Degrade` → 红 |
| J4 | `ai_ladder_executive_gate` | `plan()` 在 `{LocalOnly, RemoteAllowed} × {开, 关} × {port 有, 无}` 八格上，`Model(Executive)` 只在 (RemoteAllowed, 开, 有) 出现 | 变异：删掉 `settings.executive_enabled` 条件 → 红 |
| J5 | `ai_ladder_served_tier_decides_source` | 请求 executive、桩回 `tier: local` → `source = local_brain`、`served_tier = local` | 桩回 `tier: remote`（开关开）→ `source = executive` |
| J6 | `ai_ladder_privacy_tripwire` | 开关关、local 请求、桩回 `tier: remote` → 该步 `ok=0 privacy_tripwire`、结果被丢弃、降级 | 开关开 → 同一回复被采用 |
| J7 | `ai_boundary` | `src/ai/**/*.rs` 每个文件 `forbidden_refs` 为空；检查器自身的正对照表全部触发 | 变异：在 `src/ai/mod.rs` 加 `use crate::store::Sin90Store;` → 红（PR body 记录） |
| J8 | `ai_boundary_tables_unchanged` | §11.5 第 3 层：三个能力各跑一遍，非提议/调用/提议事件的表逐字节不变 | 随后人类 accept 一条 → 快照变化被检出 |
| J9 | `ai_calls_link_integrity` | 一次 run 的所有行 `run_id` 相同；每条 AI 提议恰好一行 `ok=1 AND proposal_id = 它`；`ok=0` 的行 `error_kind` 非空 | 变异：`record_call` 不填 `proposal_id` → 红 |
| J10 | `model_client_` | (a) `complete` 走 `call_with_timeout(125s)`（桩 transport 记录超时参数）；(b) `-32000 {kind: unavailable, retryable: false, cause: backend_config}` → `ClientError::Unavailable{false, BackendConfig}`；(c) cause 不在闭集 → `Other`；(d) `cancelled` → `Cancelled`；(e) `every_spec_error_kind_maps…` 计数 18 | (b) 的正对照：`retryable: true, cause: no_provider` 映射到另一值且 `is_retryable()` 为真 |
| J10b | `settings_ai_` | `PUT /settings/ai` 自动化 key → 403 且无事件；人类 key → 200、`setting.changed` 恰好 1 条；未知字段 → 400 | 缺行时 `GET` 返回 `false`（缺省关的正对照：删掉缺省分支 → 红） |

**T5.2.1 classify**

| # | 测试 | 断言 | 正对照 / 变异 |
|---|---|---|---|
| J11 | `classify_stub_proposes_and_data_unchanged` | inbox 一条 + 两个 Direction，桩选 `d1` → 恰好 1 条 `pending` 提议，`ops == [AssignTaskDirection(t, D1)]`，`source = local_brain`；任务 `direction_id` 仍为 NULL；人类 accept → `/today` inbox 不再含 t，`task.direction_assigned` 恰好 1 条、payload 含 `area_id` | 自动化 key accept → 403，任务仍在 inbox |
| J12 | `classify_rejects_invented_keys` | 桩回 `"d9"` / 一个真实 ULID / 多余字段 → 该步 `bad_output`，降级 | 桩回 `"d2"` → 采用 |
| J13 | `classify_reflex_history_short_circuits` | 已有同标准化标题的已归类任务 → 提议 `source = rule`，模型桩**被调用即 panic** | 标题改一个字 → 模型被调用 |
| J14 | `classify_dedup_only_valid_pending` | 第二次 run 跳过有有效挂起提议的条目 | 人经另一条提议先把它归类后（挂起提议过期）→ 不再被视为挂起；inbox 新条目照常处理 |
| J15 | `assign_task_direction_validate_` / `_apply_` | §11.2.1 A1–A5 各一正一反（scratch `ctx::tests` 为原型）；批内 `[Assign(t,d1), Assign(t,d2)]` 拒；apply 层：提议挂起期间任务被归类 → accept 422、无事件、提议仍 `pending` | 未被抢先归类 → 200 |
| J16 | `classify_remote_down_local_up` | `RemoteAllowed` + 开关开，桩：`complex` → `unavailable/no_provider`，`simple` → 成功 `tier: local` → 提议 `source = local_brain`；调用行 `executive ok=0`、`local ok=1 fallback_from=executive` | 本地桩也失败 → `source = rule` 或无提议，且 `local ok=0` 行存在 |

**T5.3.1 summarize**

| # | 测试 | 断言 | 正对照 / 变异 |
|---|---|---|---|
| J17 | `summarize_numbers_come_from_draft` | 固定事件夹具 → 周草稿；桩叙述用占位符 → 提议正文**包含** `render_facts(草稿)` 原文；正文里每个数字串都出现在 facts 块的值里 | 桩叙述写 `编码 99 小时` → 模型步 `bad_output`，提议来自 reflex（`source = rule`），正文不含 `99`；变异：删掉 `check_literal` → 红 |
| J18 | `draft_review_body_validate_` / `_cas_` | D1–D7 各一正一反；提议挂起期间人类 `PATCH` 正文 → accept 422 `StaleBase`、正文仍是人写的 | 无人改 → accept 200、正文 == 提议正文、`review.updated` 恰好 1 条、payload 形状与人类路径相同 |
| J19 | `summarize_rejects_non_draft_or_non_weekly` | finalized → 409；daily → 400；定稿后再 accept 旧提议 → 422 `ReviewNotDraft` | draft + weekly → 202 |

**T5.4.1 propose**

| # | 测试 | 断言 | 正对照 / 变异 |
|---|---|---|---|
| J20 | `propose_proposals_validate_and_apply` | P(active) + W(planning) 夹具 → 至多 3 条提议，全部经 `AiSink::submit` 的提交前校验；逐条人类 accept 成功 | 把 P 转成 reviewing 后再触发 → 无 carry 提议 |
| J21 | `propose_invalid_is_rejected_not_half_applied` | 手工构造 `[CarryOverTask(真实任务, W), ReorderTasks(W, [不存在的任务])]` → `AiSink::submit` 拒（validate 放过 reorder 时 accept 拒），库逐字节不变（无 carried_over、无新任务） | 去掉不存在的任务 → 通过 |
| J22 | `ai_allowed_ops_per_capability` | 以 propose 身份提交含 `AssignTaskDirection` 的提议 → `SinkError::Invalid`，无行；三个能力的允许集各一正一反 | 变异：`allowed_ops` 返回全集 → 红 |

**T5.5.1 真实挂载**

| # | 判据 | 期望 | 正对照 |
|---|---|---|---|
| J23 | 断网 classify | 包 A（正式 manifest，`local_only`）：`OMLX_URL` 指本地 Python 桩、无远端 → classify 产出 `source = local_brain` 提议。包 B（测试 manifest，`remote_allowed` + 开关开）：`OLLAMA_URL` 指一个**被内核标成 Remote 且连不上**的地址（ME4-S2 §2.3：`http://[::ffff:127.0.0.1]:<关闭的端口>`）、`OMLX_URL` 指本地桩 → 仍产出 `source = local_brain` 提议，调用行 `served_tier = local` | 停掉本地桩 → 无 `local_brain` 提议，调用行出现 `unavailable.no_provider` |
| J24 | 来源一致 | `SELECT count(*) FROM sin90_proposals p JOIN sin90_ai_calls c ON c.proposal_id = p.id AND c.ok = 1 WHERE p.source NOT IN ('local_brain','executive','rule') OR p.source != CASE WHEN c.engine = 'reflex' THEN 'rule' WHEN c.served_tier = 'remote' THEN 'executive' ELSE 'local_brain' END` = 0，且 AI run 期间产生的每条提议都能 join 到恰好一行 | 在库的副本里把一行 `source` 改掉 → 同一查询 = 1 |
| J25 | AI 期间无直写 | 挂载模式跑 J8 的表快照（经 daemon 真实端口触发三个能力） | 同上一条的 accept 正对照。注：tasks.md 原句「直写路由调用数 0」在进程内 AI 下**恒真**（AI 不经 HTTP），不构成判据，故以表快照代替 |
| J26 | 真 oMLX 冒烟 | `#[ignore]` 手动：`~/.omlx/models` 下的模型，三个能力各一次，记录 `model_id`、延迟、是否 `bad_output` | —— |

另加一条 manifest 钉子（T5.1.1）：`manifest_declares_models_local_only`——解析 `domain-os.yml`：`kernel_capabilities` 含 `models`、`requires_models == []`、`model_access` 缺省或 `local_only`；Q1 拍板改为 `remote_allowed` 时这条测试**必须**跟着改（让产品决定在代码里留痕）。

### 11.8 自审

- **两个 Op 的范围是否过窄**：`AssignTaskDirection` 只收 inbox、`DraftReviewBody` 只收 draft——都是故意的；放宽都应该以「加 CAS 字段」的方式做，而不是去掉前置条件（§2 #17 复审触发条件）。
- **`ValidationCtx` 又加宽了**：推翻了 §3.3 自己写的「唯一一次」。代价在本 crate 内；T5.2.1 一次加齐三个方法，避免 T5.3.1 再加一次。
- **提交前校验只给 AI 路径**：两条提交路径行为不同，是一处不对称（F-2 / R7）。选它是因为改 HTTP 路径会改变已测行为，而 M5 的目标只需要 AI 路径。
- **「executive」被如实降格**：它不是「用远端模型」，而是「允许内核按 complex 路由」；`source` 按实际服务层级定。§11.3.2 写明了 `remote_allowed` 的代价，没有把用户开关写成隐私保证。
- **数字保证的措辞**：§11.4.2 末尾的「准确说法」只声称机制做得到的部分。
- **判据是否会空转**：J3 用穷尽 `match`、J7 带检查器正对照、J8/J25 有 accept 正对照、J13 用「被调用即 panic」的桩、J24 在篡改副本上变红——每条关键判据都有让它响的办法。
- **scratch 覆盖了什么**：两个 Op 的 wire 形状与校验矩阵（含批内叠加）、引擎梯的计划与降级/中止/绊线/来源映射、run future 的 `Send`、classify 的 schema 与复核、summarize 的 facts 渲染与叙述复核、边界检查器。**没覆盖**：store 侧 `build_snapshot`/`apply_op` 的 SQL、HTTP 路由、adapter 的 `ClientError` 扩展——它们要改真实 crate，属于实现。

### 11.9 残余风险（接受，写明谁来盯）

| # | 风险 | 为什么接受 / 缓解 |
|---|---|---|
| R1 | 叙述里的数量表达漏网：`近半`、`翻倍`、`seven`、`dozen`、罕见数词写法 | 数字块由程序生成是硬保证；叙述复核是启发式。J17 的负对照证明主要路径会响；真 oMLX 冒烟（J26）里人工看一遍叙述 |
| R2 | 文本结构测试可被刻意绕过（在非 `ai/` 模块写一个包装再让 `ai/` 通过 trait 以外的方式拿到） | J8/J25 行为快照兜底；真正的编译期保证要拆 crate，留给 TS.1.1 评估 |
| R3 | `remote_allowed` 包里，`local` 调用在本地不可用时可被内核送到远端 | 正式包 `local_only`（🟡 Q1）；绊线只能事后发现。根治需要 Agent24 给 `_a24/model/complete` 加一个**只能收窄**的逐次隐私字段——作为向 Agent24 的 followup 建议提出，本文不替它决定 |
| R4 | summarize 的数字在提议挂起期间变旧 | 人重新触发；v1 不在 accept 时重算 |
| R5 | 没有拒绝路由，过期提议永久 `pending`、列表越来越长 | 去重只看「仍有效」的挂起提议，过期的不挡新 run；拒绝路由见 🟡 Q6 |
| R6 | 调用记录写失败只 `warn!`，「可追溯」尽力而为 | 与业务写同库同盘，写失败通常意味着提议也写不进去；J9 在正常路径上钉住完整性 |
| R7 | HTTP `POST /proposals` 仍不做提交前校验（F-2），两条路径不对称 | 已登记，不在 M5 范围；accept 时的校验两条路径一致 |
| R8 | 人类 `PATCH /reviews/{id}` 没有正文上限，AI 路径有 64 KiB | 不同入口不同上限；人类路径的上限另议 |
| R9 | reorder 与 carry 分开批时顺延任务 `sort_key = 0` 与首位并列 | 显示顺序上的小瑕疵，不影响正确性 |
| R10 | 本地模型不遵守 `json_schema` 时 classify/summarize/propose 的模型步总是 `bad_output` | 降级到 reflex，不丢功能；J26 冒烟暴露实际遵守率 |
| R11 | run 状态只在内存里，重启后 `GET /ai/runs/{id}` 只剩调用记录 | 调用记录与提议都持久；run 只是观察窗口 |
| R12 | 候选截断（classify 40 个 Direction、summarize 50 个标题）可能漏掉正确答案 | ⚖️ 值；方向超过 40 个的用户再调 |

### 11.10 交给实现的接口清单（按 T5.x）

**T5.1.1 引擎梯 + 调用记录**
- `src/ai/mod.rs`、`src/ai/ports.rs`：`Capability`、`Engine`、`ServedTier`、`ModelAccess`、`AiSettings`、`Complexity`、`ModelRequest`、`ModelReply`、`UnavailableCause`、`ModelFailure{…}::action()/kind_str()`、`LadderAction`、`ModelPort`、`AiSink{submit(cap, p), record_call}`、`AiReadModel`（首批方法）、`AiCallRecord`、`SinkError`、`ReadError`。
- `src/ai/ladder.rs`：`plan()`、`run_item()`、`source_for()`、`tripwire()`、`Outcome`。
- `src/store/ai_port.rs`：`impl AiReadModel for Sin90Store`、`impl AiSink for Sin90Store`（`submit` = `allowed_ops` + `build_snapshot` + `validate` + 插入，一个 `BEGIN IMMEDIATE`）。
- 迁移（max+1）：§11.6。
- `src/adapter_agent24/clients/model.rs`：`ModelClient::new(&Arc<KernelClients>) -> Option<Self>`（前缀 `_a24/model/`）、`complete(&ModelRequest) -> Result<ModelReply, ClientError>`（`call_with_timeout(125s)`，不带 `request_id`）、`impl ModelPort for ModelClient`；`clients/error.rs`：`Unavailable{retryable, cause}`、`Cancelled`，`map_rpc_error` 与两个谓词、计数测试 17 → 18。
- `domain-os.yml`：`kernel_capabilities` 加 `models`；`const MODEL_ACCESS`（`include_str!` 解析）。
- HTTP：`GET|PUT /settings/ai`、`GET /ai/runs/{run_id}`；run 注册表（内存 LRU 64）；每能力单飞；进程内模型信号量 2。
- 测试：J1–J10b、manifest 钉子。

**T5.2.1 classify**
- `Sin90Op::AssignTaskDirection`；给 `Sin90Op` 加 `#[serde(deny_unknown_fields)]`（F-1）。
- `ValidationCtx` 一次加齐 `direction_status` / `task_direction` / `review_snap`；`Working` 加两张叠加表；`ProposalError` 八个新变体。
- `DbSnapshot` / `build_snapshot` 载入；`apply_op` 分支（`require_task_week_open` + CAS UPDATE + `task.direction_assigned`）。
- `src/ai/classify.rs`：`candidate_keys`、`schema`、`parse`、`normalize_title`、R1/R2；`AiReadModel::{inbox, direction_candidates, title_history, pending_targets}`。
- `POST /ai/classify`。测试 J11–J16。
- 给 T4.3.1 的注记：按 Direction/Area 统计完成任务要折叠 `task.direction_assigned`。

**T5.3.1 summarize**（依赖 T4.3.1 的周草稿函数）
- `Sin90Op::DraftReviewBody`、`MAX_REVIEW_BODY_BYTES`、`body_sha256`；`apply_op` 分支（`review.updated`，payload 同人类路径）。
- `src/ai/summarize.rs`：`WeeklyDraft`（或直接用 T4.3.1 的类型）、`Fact`、`facts`、`render_facts`、`fill_narrative`、`compose_body`；`AiReadModel::{review, weekly_draft, done_titles}`。
- `POST /ai/summarize`。测试 J17–J19。

**T5.4.1 propose**
- `src/ai/propose.rs`：reflex 排序与顺延规则、模型 schema 与复核（排列补全、键集）；`AiReadModel::{week_tasks, previous_open_week, rhythm_alloc}`。
- `POST /ai/propose`。测试 J20–J22。

**T5.5.1 验收**
- `tests/agent24_mount_blackbox.rs`：包 A / 包 B（测试 manifest 变体）、Python 模型桩、J23–J25；J26 手动冒烟记录进 PR body。

### 11.11 待用户拍板的产品问题（🟡；文中占位均为最保守取值，不是结论）

| # | 问题 | 选项 | 文中占位 |
|---|---|---|---|
| Q1 | Sin90 正式包要不要声明 `model_access: remote_allowed`？ | (a) 不声明：executive 在正式包里不可达，本地调用由内核保证不出本机；(b) 声明：可以用远端，但**所有** AI 调用都失去内核的 LocalOnly 保证（§11.3.2）；(c) 先向 Agent24 提「逐次收窄」字段，落地后再声明 | (a) |
| Q2 | executive 用户开关的默认值 | 关 / 开 | 关 |
| Q3 | AI 的触发频率 / 是否自动触发 | 仅手动；`/capture` 之后自动 classify；复盘 Routine 到点（T4.3.2 建草稿后）自动 summarize；周进入 planning 时自动 propose；定时（如每日一次） | 仅手动 |
| Q4 | 没有合适 Direction 时，classify 要不要提议**新建** Direction（需要解决批内前向引用） | 不要 / 要 | 不要（任务留在 inbox） |
| Q5 | reflex 产出的提议记 `source = rule`，并据此改 §M5 验收原文（§2 #23） | 同意 / 坚持只允许 `local_brain`/`executive`（则 reflex 不产提议，只在 run 结果里给「建议」文本） | 同意 |
| Q6 | 要不要在 M5 加 `POST /proposals/{id}/reject`（人主动拒掉 AI 提议） | M5 加 / 以后 | 以后 |
| Q7 | 人已经写过的复盘草稿，summarize 能不能提议整体改写 | 能（CAS 防覆盖）/ 只在正文为空时 | 能 |
| Q8 | classify 模型 `confidence = low` 时要不要仍给提议 | 不给 / 给（`rationale` 标注低置信） | 不给 |

### 11.12 附录：scratch crate 与 `cargo` 记录

路径：`/private/tmp/claude-502/-Users-jason-Dev-auraai-Agent24/977deb42-1aba-448f-95e7-5bae2dee6fd4/scratchpad/t501-check/`（`sin90 = { path = "<本 worktree>" }`，另依赖 `serde`/`serde_json`/`sha2`/`hex`/`thiserror`/`tokio`）。模块：`ops.rs`（§11.2 wire + F-1）、`ctx.rs`（§11.2.3 校验矩阵与叠加）、`ports.rs`（§11.3/§11.5 端口、梯、失败表、来源映射）、`classify.rs`、`summarize.rs`、`boundary.rs`。

```
$ cargo check --all-targets
    Checking sin90 v0.5.0 (/Users/jason/Dev/auraai/sin90-F5.0)
    Checking t501-check v0.0.0 (…/scratchpad/t501-check)
    Finished `dev` profile [unoptimized + debuginfo] target(s)
$ cargo test
running 24 tests
… ops::tests::existing_sin90op_silently_accepts_unknown_fields ... ok        (F-1 实测)
… ctx::tests::draft_body_cas_chain_and_rejections ... ok
… ports::tests::remote_down_local_up_is_local_brain ... ok
… ports::tests::run_item_future_is_send ... ok
… summarize::tests::literal_numbers_are_rejected_positive_control ... ok
… boundary::tests::positive_controls_each_trip ... ok
test result: ok. 24 passed; 0 failed
```
