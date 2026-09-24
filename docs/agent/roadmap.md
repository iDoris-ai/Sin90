# Sin90 Roadmap — Milestone → Feature

> 「未来要做什么」。怎么做 + 验收见 [`tasks.md`](tasks.md)；数据模型裁决的权威是 [`../DESIGN-LIFEOS.md`](../DESIGN-LIFEOS.md)
> （本文件不重复它，只把它的 M3–M5 拆成可执行的 Feature）。记录日期：2026-09-23。
>
> **跨仓库**：本轮是 Agent24 的 **ME-4** 的一部分（`iDoris-ai/Agent24` `docs/agent/PLAN-ME4-OS-CAPABILITIES.md`）。
> Sin90 的 M3 依赖 Agent24 的调度回调（ME4-M1），M5 依赖推理回调（ME4-M4a）；MS 依赖 `agent24-os-sdk`（ME4-5.1.2）。

**已完成**：M0（`0d66f24`）· M1 Capture & Today（`4032e82`）· M2 Work Pack（`8056ade`）· 真实挂载验证（`ab66b37`）· #1 accept 只认人类 key。

```
M0' 清账 ─► M3 Routine & Rhythm ─► M4 Review & Markdown ─► M5 AI v1 ─► MS 迁到 agent24-os-sdk
              ▲ Agent24 ME4-M1                                ▲ Agent24 ME4-M4a     ▲ Agent24 ME4-5.1.2
```

## M0' — 起跑前清账

- **F0.1 合并已批准 PR** —— #2 actor key 持久化 / #3 store 不变式 / #4 事件有界 + carry-over。
- **F0.2 工程底座** —— GitHub Actions CI（fmt/clippy/test）；README/DEVELOPMENT/STATUS 里的陈旧描述改正。
- **F0.3 Codex 补审** —— M0/M1/M2/挂载修复只做过自审（尤其 actor-key 门禁 `ab66b37`）。

## M3 — Routine & Rhythm（DESIGN §M3）

**目标**：「每周 3 次运动」这种重复节律能被表达、到点真的触发，且 daemon 重启不重复注册。

- **F3.1 Routine 实体** —— 表 + 状态机 `active/paused/retired` + 事件 + 路由。
- **F3.2 内核能力适配** —— 握手声明 `events/memory/approval/scheduler`；调度/记忆/审批的类型化客户端；`/_a24/scheduler/fired` 接收路由。
- **F3.3 outbox 幂等对账** —— Routine 变更在同一事务写 `sin90_outbox`；对账器把期望状态落到内核；启动全量对账。
- **F3.4 Rhythm 开放** —— Rhythm 的创建/查询路由；调整仍只走 `AdjustRhythm` 提议门。
- **F3.5 真实挂载验收**。

## M4 — Review & Markdown（DESIGN §M4）

**目标**：复盘有地方写、有草稿可起步，而草稿里的数字**只来自事件回放**。

- **F4.1 Review 三型路由** —— daily/weekly/rhythm 的草稿/编辑/定稿。
- **F4.2 `body_ref` 单向 Markdown 外置** —— 定稿写成 `.md`，SQLite 仍是真相。
- **F4.3 周复盘草稿** —— `GET /review/weekly/draft`，纯事件回放；`Routine{kind:review}` 到点自动生成草稿。
- **F4.4 定稿进内核记忆** —— 定稿摘要经 outbox 写 `_a24/memory/private/remember`（M5 的上下文来源）。

## M5 — AI v1（DESIGN §M5，只三个能力）

**目标**：AI 帮我分类、总结、排期，但每一项改动都是等我批准的提议。

- **F5.0 设计补丁** —— classify/summarize 需要的新 Op（如给任务指派 Direction、写复盘正文）先进 DESIGN §2 表，送 Codex。
- **F5.1 引擎梯** —— reflex（规则）→ local（`_a24/model/complete`，LocalOnly）→ executive（远端，需 manifest 声明 + 用户开关）；每次调用记 `sin90_ai_calls`。
- **F5.2 classify** —— inbox 条目 → Area/Direction 提议。
- **F5.3 summarize** —— 事件 + M4 草稿 → 复盘正文提议。
- **F5.4 propose** —— 排期建议（`ReorderTasks`/`CarryOverTask`/`CreateTasks`）。
- **F5.5 验收** —— 全部走提议门；断网 classify 仍可用。

## MS — 迁到 `agent24-os-sdk`

- **FS.1** 用 SDK 替换 `src/adapter_agent24/` 手写握手与客户端；挂载黑盒全部判据不变。

## 不在本轮

- M6 Life Packs（`/packs/install` 已提前有一个非幂等的种子版本，按 DESIGN §M6 重做留到下一轮）。
- M7+ Projection / 可视化。
