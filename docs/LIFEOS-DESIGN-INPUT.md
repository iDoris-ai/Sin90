# Sin90 → Personal Life OS：设计输入记录（2026-09-20）

> 本文档记录用户在 2026-09-20 提出的 Sin90 产品方向构想、参考的外部项目、以及与 Agent24 T11（Sin90 迁出内核）现状的关系。目的：把这次讨论的全部信息落成文档，避免重复口述。**这是设计输入，不是已冻结的设计决策**——具体架构取舍见文末"结论与下一步"。

## 0. 背景：两个 Sin90 codebase 的澄清（2026-09-20 核实）

讨论开始时存在一个信息不对称，这里先钉死事实，避免以后重复踩坑：

| | `iDoris-ai/Sin90` | Agent24 内核里的 Sin90 | `MushroomDAO/Sin90` |
|---|---|---|---|
| 状态 | **骨架/模板**，`README`/`docs/STATUS.md` 原话："作为独立包被 Agent24 装载 ❌ 要等 Agent24 v0.5.0" | **成熟实现**，2026-08 SPIKE-00 验证过：`agent24-sin90`(纯域 crate,6 实体 6 状态机)+`agent24-sin90-store`(独立 `sin90.db`,CAS 幂等)+`agent24-sin90-os`(HTTP 路由 `/api/v1/sin90/*`),PR #99-#106 全部合入 main | 不相关的另一个仓库,只有 License/README,无代码 |
| 数据模型 | 无(桩) | Direction/Rhythm/Week/Task/ScheduleBlock/Review/AttentionBudget,事件溯源+回放对账已验证 | 无 |
| 本地对应目录 | 无本地 clone(本次讨论新建于 `/Users/jason/Dev/auraai/sin90-design`) | 就是 Agent24 主仓库 | `~/Dev/mycelium/Sin90`——**注意这不是同一个仓库**,remote 指向 `MushroomDAO/Sin90`,内容是空的,不要跟 `iDoris-ai/Sin90` 混淆 |

**这次讨论的起点是一次误判**：外部 AI（ChatGPT）分析了 `iDoris-ai/Sin90` 这个骨架仓库，得出"当前模型太薄，没有完整执行闭环"的结论——这个结论只对骨架仓库成立，Agent24 内核里那份实现闭环是有的（Direction→Rhythm→Task→Proposal→Event，CAS 幂等、增量回放==全量重建都有测试）。**T11 的原始定义就是把内核这份成熟实现搬进 `iDoris-ai/Sin90` 骨架仓库，包装成进程外模块，七条路由行为不变**（见 Agent24 `docs/agent/PLAN-OOP-OS-AND-BACKLOG.md` T11 行）。`iDoris-ai/Sin90` 的 `docs/STATUS.md` 里写的阻塞条件（等 Agent24 v0.5.0 的进程外支持）——**ME-3 已于 2026-09-20 整体收口，这个阻塞条件已经解除**，`STATUS.md` 需要更新。

## 1. 外部参考项目（用户指定，供设计时参考）

- **Daniel Miessler / LifeOS** — https://github.com/danielmiessler/LifeOS
  最值得借鉴：**Current State → Ideal State** 主轴、TELOS、长期上下文、AI Skills、Pulse。不建议照搬整套复杂 AI Harness。
- **quanru / obsidian-lifeos** — https://github.com/quanru/obsidian-lifeos
  最值得借鉴：Daily/Weekly/Monthly/Quarterly 周期机制、PARA、Quick Capture。不建议把 Obsidian 当 Sin90 核心运行时。
- **luneth90/lifeos**（开源发布公告，无直接 URL，按名称检索）
  最值得借鉴：Local-first、Markdown、Agent 无关（人生数据是自己的，执行者可以换：Claude Code/Codex/OpenCode 都只是执行者）、MCP、结构化 Memory。不建议把"学习知识库"当整个 Life OS。
- **jasonkneen / tiny-world-builder** — https://github.com/jasonkneen/tiny-world-builder
  最值得借鉴：**状态模型与 UI 渲染解耦**——`world[x][z]` 真实状态 vs `cellMeshes` UI Projection，统一通过 mutation entry point 改状态。未来 Sin90 做可视化（人生地图/花园/岛屿）时可以用同样思想，**它应该是 Projection，不是数据库**。不建议现在就开始做 3D/2D 可视化——这是 M11，排在最后。

## 2. 用户自己的"五大人生系统"框架（已有构思，转录保存）

核心三原则：**先模块化，再自动化，最后精简化**。

1. **财富系统 WEALTH · 4 ACCOUNTS**：工资主账户→生活账户（日常）、主账户→投资账户（自动转账固定比例）、主账户→应急资金（固定比例）。预算上限超出即冻结。季度看一次资产曲线。
2. **学习系统 LEARNING · IN → OUT**：闭环 = 输入→处理→输出→反馈，少任何一环不算学完。Daily 30 分钟阅读；Weekly 整理一篇输出；Monthly 一次分享；Yearly 12 本书笔记装订。**没有输出的阅读是消费，不是学习。**
3. **工作系统 WORK · 3 + 90 + REVIEW**：看板三栏（本周做/排队中/已完成）；每天 3 件"今天必须完成"；每天 90 分钟不被打断的深度时间；每周五复盘 1 小时；清单外的当场不接。
4. **健康系统 HEALTH · SLEEP · MOVE · FUEL**：三支柱——睡眠（上下床时间 ±30 分钟全年不变）、运动（每周 3 次 ×30 分钟+）、饮食（少加工慢碳水，不节食只替换）。
5. **关系系统 RELATION · 3 LAYERS**：核心层（3-5 人）/深层（15-25 人）/外圈（按需）。Weekly 一次主动联系；Monthly 一次线下；Quarterly 一次深谈；Yearly 一次通讯录清理+关系复盘。

**这五个系统在架构上应该是 Sin90 Core 之上的 Area/Pack，不是 Core 本身**——不同用户可以有完全不同的 Area 划分（比如 Career/Capability/Creation/Relationship/Finance/Life 这种六分法），Sin90 不用为此改代码。

## 3. 提议的核心数据模型重构（GPT 生成，用户认可方向，未冻结细节）

### 3.1 三层循环升级为对象图

原有 Direction→Rhythm→Task 三层保留，但建议补上执行/复盘环节，形成真正的闭环而不是单向计划：

```
Direction（我要去哪）→ Rhythm（什么节奏）→ Task（下一步）→ Execution（实际发生）→ Review（偏差）→ 调整方向
```

对象图建议扩展为：

```
Area → Direction → Goal → Project → Task → Execution → Review
```

`Rhythm` 横向穿过这些对象：`Routine` / `ScheduleBlock` / `ReviewCycle` / `Reminder`。

### 3.2 Proposal 的语义要收紧

**Proposal ≠ Task。** 明确定义：AI 想改变用户 Life OS 状态时，必须先生成 Proposal，不能直接改真实状态；用户 accept 之后才落地成 `ScheduleBlock`/`Project` 更新。（现有内核实现已经是这个语义——`sin90_proposals` 表 + CAS 幂等 apply，这条不是新要求，是确认延续。）

### 3.3 存储：Hybrid，不是纯 Markdown

维持已有决定（Sin90 自己存数据，不用 Agent24 Memory），但不要全部存 Markdown：

- **SQLite = operational source of truth**：Area/Direction/Goal/Project/Task/Routine/ScheduleBlock/Execution/Metric/Review/Proposal。
- **Markdown**：长文本复盘、研究笔记、人生原则、年度总结、AI Report、项目总结。

### 3.4 补一个基础抽象：Event Log

不能只存状态快照（`Task.status = completed`），要记录事件序列（`task.created`→`task.scheduled`→`task.started`→`task.completed`），否则无法回答"这周想了很多 Career 的事，但真正执行了多少"这类问题。**现有内核实现已经是事件溯源架构**（`sin90.*` 事件、`attention` 纯回放）——这条也不是新要求，是确认延续，重构时不能退化成只存快照。

### 3.5 Agent24 解耦原则

Sin90 的 Domain Logic **不 import Agent24**，只有一个 `adapter-agent24/` 知道 Agent24 是谁——这样 Sin90 不会被 Agent24 的开发进度锁死，可以先 standalone 跑起来。**这条原则需要跟 T11 的实际定义核对**：T11 的验收标准是"内核里删掉 `agent24-sin90-os` 后，装上独立仓库产出的包，七条路由行为不变"，意味着 Sin90 最终仍然是**跑在 Agent24 daemon 里的进程外模块**（通过 Agent24 的进程外协议挂载），不是完全脱离 Agent24 独立部署的服务——GPT 提议的"Standalone 现在就跑 / Agent24 Adapter v0.5 后接"这个双轨思路，需要跟 T11 的既有验收标准对齐，不能两者打架。

### 3.6 建议的渐进式里程碑（GPT 原始提议，未核对是否与内核已有实现重复）

| 阶段 | 交付 | 完成后能做什么 |
|---|---|---|
| M0 Core | 数据模型 + SQLite + Event（Area/Direction/Project/Task/Event/Proposal） | 有可靠数据底座 |
| M1 Capture + Today | Inbox/Capture/Today/Done | 每天真的开始用 |
| M2 Work Pack | Goal→Project→Task | 完整管理工作 |
| M3 Direction | Area/Direction/Goal | 从 Todo 升级到 Life OS |
| M4 Rhythm | Routine/Schedule/Calendar | 建立长期节奏 |
| M5 Review | Daily/Weekly/Monthly | 形成反馈闭环 |
| M6 AI | 分类/Proposal/Review Agent | AI 真正参与管理 |
| M7 Learning Pack | Input→Process→Output | 管理学习 |
| M8 Health Pack | Sleep/Move/Fuel/Metrics | 管理健康 |
| M9 Relationship Pack | People/Touch/Review | 管理关系 |
| M10 Wealth Pack | Account/Flow/Budget/Review | 管理财富 |
| M11 Visual World | 2D/3D Life Map（tiny-world-builder 思路） | 人生世界可视化 |

建议第一个垂直模块做 **Work**（不是 Wealth/Health），理由：Work 能一次把 Direction/Goal/Project/Task/Schedule/Execution/Review/AI Proposal 全部跑一遍，是 Core 的压力测试。

**AI v1 只给三个能力**：classify / summarize / propose，不直接改用户日程，只产出 Proposal。

## 4. 结论与下一步（2026-09-20，用户拍板）

- **T10（Cos72）暂停，优先做 Sin90 迁出内核**。
- **T11 的既有搬家范围（内核成熟实现 → `iDoris-ai/Sin90`，七条路由不变）是地基，不推翻**——已经验证过的 Direction/Rhythm/Task/Proposal/Event 模型不重做。
- **GPT 的 Life OS 构想（Area/Goal/Project 分层、Rhythm 横切、Hybrid 存储、M0-M11 分期）作为在这个地基上的扩展方向去评估，不是替换方案**——具体哪些立即采纳、哪些需要先跟已有实现核对是否重复/冲突（尤其 3.4 Event Log、3.2 Proposal 语义，内核里可能已经满足），由下一步的设计任务核实后给出取舍。
- 下一步：**用 Opus 做一次设计+里程碑文档任务**——核对本文档 §3 的每一条构想跟内核已有实现是否重复/冲突/需要改造，产出正式设计文档 + 里程碑拆解，先定义到"M0：可加载的最基础个人 OS"这个粒度，不直接跳过设计写代码。
