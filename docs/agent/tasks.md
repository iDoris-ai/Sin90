# Sin90 任务台账 — Task

> 前置：[`roadmap.md`](roadmap.md)（M→F）·[`architecture.md`](architecture.md)·[`spec.md`](spec.md)·数据模型权威 [`../DESIGN-LIFEOS.md`](../DESIGN-LIFEOS.md)
> **本文件是 Sin90 唯一的执行状态来源**。跨仓库的门在 Agent24 `docs/agent/tasks.md`「ME-4 台账」（`ME4-M2 门` 等）。
> 状态：BACKLOG · READY · IN_PROGRESS · BLOCKED · PR_OPEN · CHANGES_REQUESTED · APPROVED · DONE
>
> **全局验收前置**（每个 task 都适用，不再逐条重复）：
> ```
> cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test
> ```
> **真实挂载验收**（凡是碰到 adapter / manifest / 与内核交互的 task 都要跑；需要 Agent24 main 已含对应内核能力）：
> ```
> AGENT24_CHECKOUT=../Agent24 cargo test --test agent24_mount_blackbox -- --ignored --test-threads=1
> ```
> 跑之前先把 Agent24 更新到含所需内核能力的 main：`git -C ../Agent24 fetch origin && git -C ../Agent24 merge --ff-only origin/main`。
> **验收命令不许空转**：`cargo test <过滤>` 形式的验收先跑 `cargo test <同参数> -- --list` 并断言匹配数 > 0；需要外部工具的测试工具缺失即失败。
> **新回归测试一律变异验证**：把修复改回去，确认测试变红；PR body 写明变异方式与结果。每条判据带正对照。
> **合并判据**：clestons 的 APPROVED review 的 `commit_id` == PR 当前 `headRefOid`，且 check 全 SUCCESS（main 开保护前用 `gh pr merge --squash`）。
> **台账回填**：PR 合并后 `DONE`/证据由下一个 PR 顺带回填（Agent24 PLAN-ME4 §一 第 7 条）。
> **数据模型改动先改 DESIGN §2 表**（DESIGN §10），再改代码。
> 流程：一个 task 一个分支一个 PR（`feat/`、`fix/`、`docs/`、`chore/` 前缀）；自审 → Codex 挑战 → `pre-pr-check.sh --base main` → PR → clestons。

---

## M0' — 起跑前清账

### T0.1 合并已批准的 #2 / #3 / #4  `DONE`
- **优先级**：high
- **目标**：三个已 APPROVED 的修复进 main。
- **开发范围**：逐个核合并判据（exact-head approve + check 全绿；Sin90 暂无 CI 时以 PR body 的本地测试记录为准并在台账注明）→ `gh pr merge <n> --squash`；按 #2 → #3 → #4 顺序。
  若先合的导致后面冲突：rebase、推送、**等 clestons 复审**（旧 approve 失效），不自合。
- **明确不做**：往已批准分支推任何「顺手」改动。
- **验收命令**：`gh pr list -R iDoris-ai/Sin90 --state open --json number --jq '[.[].number]'` 不含 2/3/4；main 上全局前置全绿。
- **证据**：#2 `0949d37`、#3 `bd84e9a`、#4 `f5c5443`（2026-09-24）

### T0.2 GitHub Actions CI  `DONE`
- **优先级**：high
- **目标**：每个 PR 自动跑 fmt/clippy/test（挂载黑盒是 `#[ignore]`，不进 CI）。
- **开发范围**：`.github/workflows/ci.yml`（ubuntu + macos，stable toolchain，cargo cache）。
- **依赖**：T0.1
- **验收命令**：PR 上 CI 为 SUCCESS；`gh run list -R iDoris-ai/Sin90 --limit 1 --json conclusion` = success。
- **证据**：#6 `329d5ab`；main CI 绿

### T0.3 陈旧文档改正  `DONE`
- **优先级**：low
- **目标**：README（列了不存在的 `routes.rs/store.rs/handshake.rs`）、DEVELOPMENT §59–71（说握手是 stub）、STATUS（说 `src/*` 是 stub）、DESIGN §5.4（`--module` 应为 `module`）与代码一致。
- **依赖**：T0.1
- **验收命令**：`! grep -n 'handshake.rs\|src/routes.rs\|src/store.rs' README.md`；`grep -n '"module"\|\[module\]' docs/DESIGN-LIFEOS.md`。
- **证据**：#7 `8e34767`

### T0.4 Codex 补审历史改动  `BACKLOG`
- **优先级**：high
- **目标**：`0d66f24`/`4032e82`/`8056ade`/`ab66b37` 过一次真正的对抗评审；actor-key 门禁（`ab66b37`）优先。
- **开发范围**：按 commit 送 Codex（额度 2026-09-29 19:28 才恢复；在此之前 T0.4 保持 BACKLOG，不用 Opus 代替 —— 这一条的目的就是补上「真正的外部对抗评审」）；每条发现中立裁决；真问题各开修复 PR（一个问题一个 PR）；不阻塞的进 [`followups.md`](followups.md)。
- **依赖**：T0.1（在合并后的 main 上审，避免审到已修的旧代码）
- **验收命令**：`followups.md` 有「T0.4 Codex 补审」段，逐 commit 记结论与去向；修复 PR 全部 MERGED 或登记为 followup。
- **证据**：

---

## M3 — Routine & Rhythm

### T3.1.1 Routine 存储 + 状态机  `PR_OPEN`
- **优先级**：high
- **目标**：`sin90_routines` 表与 `Routine` 实体（DESIGN §3.2：`id, area_id?, direction_id?, title, kind, cron, tz, target_count, target_minutes, status`）。
- **开发范围**：DESIGN §2/§3.2 先补 `tz` 字段（IANA，缺省 UTC）；迁移 `000N_routines.sql`；`core/types.rs` 类型；`core/transitions.rs`
  `active↔paused → retired`（retired 终态）；store 的 create/get/list/update/transition，每个状态变化同事务追加 `routine.*` 事件；
  cron 用与内核相同的 `cron = "0.15"` 语义校验（5 段补秒）。
- **明确不做**：路由、outbox、任何内核调用。
- **依赖**：T0.1
- **验收命令**：`cargo test routine_`：非法 cron/tz 被拒；retired 后任何迁移被拒；每次变更恰好一条事件（正对照：读路径零事件）。
- **证据**：#9（已 rebase 待重审）→ #10 → #11

### T3.1.2 Routine 路由  `PR_OPEN`
- **优先级**：high
- **目标**：`POST /routines`（require_human）、`GET /routines`、`GET /routines/{id}`、`PATCH /routines/{id}`（title/cron/tz/target_*）、`POST /routines/{id}/transition`。
- **依赖**：T3.1.1
- **验收命令**：`cargo test --lib http::tests::routine`（测试放在 `http::tests::routine` 子模块；先 `-- --list` 确认 > 0）：自动化 key 直写 → 403（正对照：人类 key → 201）；未知字段 → 400。
- **证据**：#12

### T3.2.0 回调通道多路复用 transport  `PR_OPEN`
- **优先级**：high
- **目标**：替换今天「一个 mutex 包住整个写请求—读响应往返」的串行 `CallbackChannel`（`adapter_agent24/mod.rs:85-101`），为并发的调度/记忆/模型调用铺路。
- **开发范围**：单 writer + 后台 reader + `id → oneshot` 分发；在途上限 64（与内核 `MAX_IN_FLIGHT_PER_CONNECTION` 一致）；调用方取消 → 发 JSON-RPC **notification** `$/cancelRequest`（params `{id}`，notification 本身不带 id；方法名以 `agent24-os-proto/src/rpc.rs:94-95` 的 `CANCEL_METHOD` 为准）并释放槽位；
  **回调连接断了就结束这一代**（内核合约 D1：一代一条连接、不许重连，见 architecture 第 5 条）：transport 判定死亡 → 关闭连接、在途调用得到「结果不确定」错误（不自动重试）→ 进程以非零码退出，由 supervisor 重启；删除现有重连逻辑；`KernelClients` 独立持有 channel，不再挂在 EventSink 上（今天 `main.rs:91-95` 只有 events 被 offer 时才保留 channel）。
- **明确不做**：新的业务客户端（T3.2.1）。
- **依赖**：T0.1
- **验收命令**：`cargo test transport_`（假 socket）：两个并发调用乱序返回各自拿到正确结果；第 65 个在途调用排队/拒绝按设计；取消后槽位释放且写出的取消帧逐字节等于 `{"jsonrpc":"2.0","method":"$/cancelRequest","params":{"id":…}}`（精确 wire 测试）；
  连接死亡 → 在途调用在有界时间内拿到错误、之后的新调用立即失败、进程退出路径被触发（测试里用可注入的退出回调代替真正 exit）；**回调连接在进程整个生命周期内都必须持有**（连接就是这一代的生命线：关掉它内核即判本代死亡并重启，空 Offer 时会重启风暴直到熔断）；Offer 决定的只是「业务代码能拿到哪些客户端」——只 offer scheduler、不 offer events 时 scheduler 客户端可用；都不 offer 时业务代码拿不到任何客户端，但连接照样持有（测试：空 Offer 时对端在 N ms 内读不到 EOF；正对照：真 drop 后读到 EOF）。变异：reader 按到达顺序而非 id 分发 → 变红。
- **证据**：#14（已 rebase 待重审）→ #15 → #16 → #17

### T3.2.1 握手声明 + 类型化内核客户端  `PR_OPEN`
- **优先级**：high
- **目标**：adapter 能调 `_a24/scheduler/*`、`_a24/memory/private/*`、`_a24/approval/*`（类型化，业务代码不见 JSON-RPC）。
- **开发范围**：`domain-os.yml` 的 `kernel_capabilities: [events, memory, approval, scheduler]`；`initialize` 的 capabilities 同步；按 `Offer.provides` 决定客户端是否可用（缺 → `None`）；
  错误 kind 映射成 Sin90 的错误类型（forbidden / rate_limited / busy / quota_exceeded / invalid_params / timeout / 其它）。
- **明确不做**：业务调用方；SDK（MS 再换）；真实挂载验收（T3.2.3）。
- **依赖**：T3.2.0；Agent24 `ME4-1.1.1`（线格式冻结）
- **验收命令**：`cargo test adapter_agent24::clients`（假 socket 回放成功 / forbidden / rate_limited / 连接死亡 → 结果不确定错误）。
- **证据**：#18 → #19 → #20

### T3.2.3 真实挂载下的 Offer 验收  `BACKLOG`
- **优先级**：mid
- **目标**：真实 agent24d 给出的 `provides` 含 `_a24/scheduler/`，三类客户端各成功一次往返。
- **依赖**：T3.2.1；Agent24 `ME4-1.5.1` 已合并
- **验收命令**：真实挂载验收命令（新增用例 `kernel_clients_roundtrip`）。
- **证据**：

### T3.2.2 fired 接收路由  `PR_OPEN`
- **优先级**：high
- **目标**：`POST /_a24/scheduler/fired` —— 内核到点投递的落点。
- **开发范围**：只在**挂载模式**注册（standalone `serve` 模式不注册 → 404，因为那里没有代理剥头，`X-A24-*` 可伪造）；
  必须有 `X-A24-Fire-Id`，否则 400；内核语义是**至少一次**、同一次到点的重试共用 fire_id，所以去重必须按 fire_id 且幂等返回 2xx；`sin90_routine_fires(fire_id UNIQUE, routine_id, scheduled_for, received_at)` 去重；
  记 `routine.fired` 事件；`/today` 增「今日到点的 Routine」段。
- **依赖**：T3.1.1、T3.2.0
- **验收命令**：`cargo test fired_`：同一 fire_id 两次 → 一行一事件（正对照：不同 fire_id 两行）；无头 → 400；standalone 模式 404。
- **证据**：#21 → #22

### T3.3.1 outbox 写入  `PR_OPEN`
- **优先级**：high
- **目标**：Routine 的每次变更在**同一事务**里写 `sin90_outbox`（`dedup_key = routine:<id>`，`desired = {key, spec, enabled}`，最新期望覆盖旧的 pending）。
- **开发范围**：先改 DESIGN §2/§4.1（DESIGN §10 规则）：outbox 状态扩为 `pending|done|failed`，加 `failure_kind TEXT NULL`、`last_error TEXT NULL`、`attempts INTEGER NOT NULL DEFAULT 0`、`next_attempt_at TEXT NULL`，
  再写迁移 `000N_outbox_failed.sql`。错误分类：**永久失败**（`forbidden`、`quota_exceeded`、`invalid_params`）→ `failed`，在 `/today` 暴露，Routine 下次变更时重置为 pending；
  **可重试**（`rate_limited`、`busy`、`timeout`、断连、`not_ready`、`draining`）→ 退避后重试。
  active → `upsert(enabled=true)`；paused → `upsert(enabled=false)`；retired → `delete`。
- **依赖**：T3.1.2
- **验收命令**：`cargo test outbox_`：同 routine 连改 3 次只剩 1 条 pending 且为最新期望；旧库迁移后既有 outbox 行状态不变；事务回滚时 outbox 无残留（变异：把写 outbox 移出事务 → 变红）。
- **证据**：#13

### T3.3.2 对账器  `BACKLOG`
- **优先级**：high
- **目标**：把 outbox 的期望状态幂等落到内核，重启不重复。
- **开发范围**：后台任务取 pending → 调 `_a24/scheduler/upsert|delete`（key = `routine.<id>`）→ 成功标 done；失败指数退避；
  启动时全量对账：`list` 拿到内核的**完整期望状态**（key、spec、enabled、user_suspended、system_disabled_reason），与本地逐字段比较：
  缺的补、多的删、spec/enabled 漂移的 upsert、`system_disabled_reason` 非空的 upsert 以恢复；**`user_suspended` 的行不去碰，并在 `/today` 标出「已被你在内核侧暂停」**；
  无内核（standalone）时 outbox 保持 pending 不报错。
- **依赖**：T3.3.1、T3.2.1
- **验收命令**：`cargo test reconcile_`（假内核）：同一 pending 被处理两次 → 内核侧仍 1 条；内核有孤儿 key → 被删；spec 漂移 → 被纠正；system_disabled → 被恢复；user_suspended → 不被改（正对照：enabled=false 且非 user_suspended → 被恢复）；rate_limited → 退避后成功；quota_exceeded → 标 failed 不再重试（正对照：rate_limited 不标 failed）。
- **证据**：

### T3.4.1 Rhythm 开放  `PR_OPEN`
- **优先级**：mid
- **目标**：UI 能建和看 Rhythm，调整只走提议门。
- **开发范围**：`POST /rhythms`（require_human，直写，allocations 校验 pct 合计 ≤ 100、direction 存在）、`GET /rhythms`、`GET /rhythms/{id}`；
  调整 = 现有 `POST /proposals` 提交 `AdjustRhythm`（不新增 Op、不新增直写调整路由）。
- **依赖**：T0.1
- **验收命令**：`cargo test --lib http::tests::rhythm`（测试放在 `http::tests::rhythm` 子模块；先 `-- --list` 确认 > 0）：建 → 提 AdjustRhythm → 人类 accept → 状态 `adjusted`；自动化 key accept → 403。
- **证据**：#8（已 rebase 待重审）

### T3.5.1 M3 真实挂载验收  `BACKLOG`
- **优先级**：high
- **目标**：DESIGN §M3 验收原文成立。
- **开发范围**：扩展 `tests/agent24_mount_blackbox.rs`（daemon 以 `A24_SCHEDULER_TICK_SECS=1` 启动）：建「每周 3 次运动」（`0 7 * * MON,WED,FRI`）→ 内核 `GET /api/v1/schedules` 该模块 1 行；
  重启 daemon → 仍 1 行且未被禁用；**正对照**：测试钩子让对账器对同一 routine 连发两次 upsert → 仍 1 行；
  另建一条测试 Routine，cron 定到**下一分钟**（Sin90 的 Routine 只有 cron，不为测试加 `At`）→ **经真实 tick 到点** → Sin90 `routine.fired` 恰好 1 条；`run_now` 仅作额外正对照；
  客户端伪造 fired → 404；pause → 内核行 `enabled=false`；用户经内核 REST 暂停 → Sin90 重启对账后仍暂停；retire → 内核行消失。
- **依赖**：T3.2.2、T3.2.3、T3.3.2、T3.4.1；Agent24 `ME4-1.5.1`
- **验收命令**：真实挂载验收命令连跑 5 次全绿。完成后回填 Agent24 台账 `ME4-M2 门`。
- **证据**：

---

## M4 — Review & Markdown

### T4.1.1 Review 路由  `PR_OPEN`
- **优先级**：high
- **目标**：`POST /reviews`（kind + 期间：daily=日期、weekly=ISO 周、rhythm=rhythm_id）、`GET /reviews`、`GET /reviews/{id}`、`PATCH /reviews/{id}`（仅 draft）、`POST /reviews/{id}/finalize`。
- **开发范围**：同期间同 kind 唯一；每个变化一条 `review.*` 事件；直写 require_human。
- **依赖**：T3.5.1（避免与 M3 并行改同一批路由文件）
- **验收命令**：`cargo test --lib http::tests::review`（测试放在 `http::tests::review` 子模块；先 `-- --list` 确认 > 0）：finalized 后 PATCH → 409；重复期间 → 409（正对照：不同期间 201）。
- **证据**：#23 → #24 → #25

### T4.2.1 `body_ref` 单向 Markdown 外置  `PR_OPEN`
- **优先级**：mid
- **目标**：定稿时把正文写成 `<data_dir>/reviews/<kind>/<period>.md`，`body_ref` 存相对路径；SQLite 仍是真相，md 只写不读。
- **开发范围**：迁移加 `body_ref TEXT NULL`；原子写（临时文件 + rename）；路径只由 kind + 期间派生（不接受用户路径）。
- **依赖**：T4.1.1
- **验收命令**：`cargo test body_ref_`：定稿后文件内容 == `body`；手改 md 后读 API 仍返回 SQLite 的值（正对照）；构造 `../` 期间 → 被拒。
- **证据**：#26

### T4.3.1 周复盘草稿（纯事件回放）  `PR_OPEN`
- **优先级**：high
- **目标**：`GET /review/weekly/draft?week=YYYY-Www` 的数字只来自事件回放（DESIGN §M4 验收）。
- **开发范围**：复用 attention 回放；按 Area/Direction 汇总时长、完成任务数、Routine 完成次数（`routine.fired` + 对应 ScheduleBlock completed）。
- **依赖**：T4.1.1
- **验收命令**：`cargo test weekly_draft_`：固定事件夹具 → 小时数逐位相等；**负对照**：绕过事件直接改 `sin90_schedule_blocks` 行 → 草稿不变。完成后回填 Agent24 `ME4-M3 门`（与 T4.4.1 一起）。
- **证据**：#27

### T4.3.2 review Routine 到点自动建草稿  `PR_OPEN`
- **优先级**：mid
- **目标**：`Routine{kind:review}` 的 fired → 若该期间无 review，建一条 draft（正文 = T4.3.1 的草稿）。
- **依赖**：T4.3.1、T3.2.2
- **验收命令**：`cargo test review_routine_`：同一期间两次 fired → 只一条 draft。
- **证据**：#28

### T4.4.1 定稿写进内核私有记忆  `BACKLOG`
- **优先级**：mid
- **目标**：定稿摘要（派生副本，真相仍在 Sin90 SQLite）经 outbox（`kind = memory.remember`，`dedup_key = review:<id>`）写 `_a24/memory/private/remember`，供 M5 做上下文。
- **依赖**：T4.2.1、T3.3.2
- **验收命令**：`cargo test` 覆盖 outbox 行；真实挂载验收里定稿后 `_a24/memory/private/recall` 能找回（精确 id 关联）。
- **证据**：

---

## M5 — AI v1（只 classify / summarize / propose）

### T5.0.1 AI v1 设计补丁  `PR_OPEN`
- **优先级**：high
- **目标**：把 M5 需要的新 Op（至少：给 inbox 任务指派 Area/Direction；写 review 草稿正文）先写进 DESIGN §2 表与 §1.3，定义校验规则，送对抗评审（Codex，额度耗尽期间为 Opus 子代理）到 approve。
- **明确不做**：代码。
- **依赖**：T4.4.1、T4.3.1；Agent24 `ME4-4.1.1`（推理回调线格式冻结）
- **验收命令**：DESIGN §2 有新行；评审方末轮 approve 记录在 PR body。
- **证据**：#30（设计冻结 v2.1）

### T5.1.1 引擎梯 + 调用记录  `PR_OPEN`
- **优先级**：high
- **目标**：`ai` 模块：reflex（规则）→ local（`_a24/model/complete`，内核强制 LocalOnly）→ executive（仅当 manifest `model_access: remote_allowed` 且用户设置开启）；每次调用写 `sin90_ai_calls`（engine、fallback_from、latency、ok）。
- **开发范围**：**AI 模块只能调 `submit_proposal`，不能调任何 store 写函数**（结构约束：AI 模块不 import 写接口；加一条编译期/grep 测试）。
- **依赖**：T5.0.1（T5.1.2 接线另列）
- **验收命令**：`cargo test ai_ladder_`：local 不可用 → 降到 reflex 并记 `fallback_from`；`cargo test ai_boundary`（AI 模块对 store 写函数引用数 = 0，正对照：故意引用 → 测试变红）。
- **证据**：#31 → #32 → #33 → #34

### T5.1.2 AI 接线（真实模型客户端 + manifest 常量）  `BACKLOG`
- **优先级**：high
- **目标**：把 T5.1.1 的 `ModelPort` 接到真实内核 `_a24/model/complete`，并落地 manifest 侧的编译期常量。
- **开发范围**（T5.1.1 回报的「留给接线任务」清单，原文在 `src/ai/mod.rs` 模块文档）：`adapter_agent24/clients/model.rs`（`ModelClient` 实现 `ModelPort`；`ClientError` 加 `Unavailable{retryable, cause}` / `Cancelled`，`map_rpc_error` 计数 17→18，即 SFU-11）；`domain-os.yml` 加 `kernel_capabilities: models`（**不**声明 `remote_allowed`，DESIGN §2 #26 硬约束）；`remote-allowed-manifest` feature 与 `domain-os.remote-allowed.yml`（只给测试包 B）；`MODEL_ACCESS` 常量与 `sin90 print-model-access` 子命令；判据 J10、J10c、J23b。
- **依赖**：T5.1.1；T3.2.1（Transport 线的客户端）；Agent24 推理回调合并（ME4-4.2.2b2）。**两条 stacked 线合并后从 main 开工。**
- **验收命令**：设计 §11.7 的 J10 / J10c / J23b。
- **证据**：

### T5.2.1 classify  `PR_OPEN`
- **优先级**：high
- **目标**：inbox 条目 → 指派 Direction 的提议（新 Op `AssignTaskDirection`，见 DESIGN §11）；`source` 由 store 按实际产出引擎推导（reflex → `rule`，本地模型 → `local_brain`，远端 → `executive`，依据内核返回的 `result.tier`）。
- **依赖**：T5.1.1
- **验收命令**：`cargo test classify_`（桩模型）：产出 pending 提议且数据未变；人类 accept 后任务离开 inbox。
- **证据**：#35 → #36 → #37（Opus 3 轮；Codex 未审）

### T5.3.1 summarize  `BLOCKED`
- **优先级**：mid
- **目标**：事件 + T4.3.1 草稿 → 复盘正文提议。
- **依赖**：T5.1.1、T4.3.1
- **验收命令**：`cargo test summarize_`：提议里的数字与 T4.3.1 草稿一致（模型只负责措辞，数字由事件给）。
- **证据**：依赖 T4.3.1（#27）合并；等线合并后再叠

### T5.4.1 propose  `IN_PROGRESS`
- **优先级**：mid
- **目标**：排期建议（`ReorderTasks` / `CarryOverTask` / `CreateTasks`）作为提议。
- **依赖**：T5.1.1
- **验收命令**：`cargo test propose_`：提议通过 `validate`；非法建议（引用不存在的任务）被 validate 拒绝而不是半应用。
- **证据**：分支 `feat/t5.4.1-propose`（叠在 #37 上），Opus 第 3 轮评审中

### T5.5.1 M5 验收  `BACKLOG`
- **优先级**：high
- **目标**：DESIGN §M5 验收原文成立。
- **开发范围**：挂载黑盒里 `OMLX_URL` 指向本地桩；远端 provider 不可达（断网模拟）→ classify 仍产出提议；
  `SELECT count(*) FROM sin90_proposals WHERE source NOT IN ('local_brain','executive','rule')` 对 AI 产出为 0；AI 期间直写路由调用数 0。
- **依赖**：T5.2.1、T5.3.1、T5.4.1
- **验收命令**：真实挂载验收命令全绿；另附一次真 oMLX 手动冒烟记录（`~/.omlx/models` 下的模型）。完成后回填 Agent24 `ME4-M4b 门`。
- **证据**：

---

## MS — 迁到 `agent24-os-sdk`

### TS.1.1 adapter 换成 SDK  `BACKLOG`
- **优先级**：high
- **目标**：`src/adapter_agent24/` 的手写握手/客户端换成 `agent24-os-sdk`（git 依赖 + tag `agent24-os-sdk-v0.1.0`）。
- **依赖**：Agent24 `ME4-5.1.2c`
- **验收命令**：`git diff --stat origin/main -- src/adapter_agent24` 净减；真实挂载验收（M3/M4/M5 全部判据）全绿。完成后回填 Agent24 `ME4-5.2.1`。
- **证据**：

---

## 依赖链

```
T0.1 ─┬─ T0.2 / T0.3 / T0.4
      ├─ T3.1.1 ─ T3.1.2 ─ T3.3.1 ─┐
      ├─ T3.4.1                     ├─ T3.3.2 ─┐
      └─ T3.2.0 ─┬─ T3.2.1 ─┴─ (T3.3.2, T3.2.3)  ├─ T3.5.1
                 └─ T3.2.2（+T3.1.1）────────────┤ ─ T4.1.1 ─ T4.2.1 ─ T4.4.1 ─ T5.0.1 ─ T5.1.1 ─ T5.2/5.3/5.4 ─ T5.5.1 ─ TS.1.1
                                               │           └─ T4.3.1 ─ T4.3.2
        Agent24: ME4-1.1.1 → T3.2.1 · ME4-1.5.1 → T3.2.3/T3.5.1 · ME4-4.x → T5.x · ME4-5.1.2c → TS.1.1
```
