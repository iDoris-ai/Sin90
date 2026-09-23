# Sin90 架构 — 技术骨架与不可破边界

> 数据模型/状态机的裁决在 [`../DESIGN-LIFEOS.md`](../DESIGN-LIFEOS.md)；本文件只写**运行形态与边界**。记录日期：2026-09-23。

## 运行形态

- 独立 Rust 二进制 `sin90`，两种模式：`serve --data-dir`（standalone，调试用）/ `module`（由 agent24d spawn，进程外领域 OS）。
- 挂载后内核把 `/api/v1/sin90/*` 经受约束代理转到 Sin90 的 UDS（fd `A24_LISTEN_FD`）；Sin90 经回调 socket（`A24_CALLBACK_SOCK`）调内核。
- 存储：`~/.agent24/os/sin90/` 下自己的 SQLite（WAL、FK 开）是**业务真相的唯一来源**；只有显式的**派生摘要**（如复盘定稿摘要）会写进 Sin90 自己的内核 private-memory 分区，那份副本不是真相。
- 分层：`core/`（纯类型 + 状态机 + 提议校验，无 IO）→ `store/`（sqlx，每次状态变化 `BEGIN IMMEDIATE` + 同事务追加事件）
  → `http/`（axum 路由 + actor 门）→ `adapter_agent24/`（唯一知道内核线协议的地方，MS 后换成 SDK）→ `ai/`（M5 新增）。

## 与内核的契约（ME-4 起）

| 能力 | 用途 | 方法 |
|---|---|---|
| events | 状态变化转发给内核事件总线 | `_a24/events/emit` |
| scheduler | Routine 的 cron 落到内核 | `_a24/scheduler/{upsert,delete,list}` + 接收 `POST /_a24/scheduler/fired` |
| memory | 复盘定稿的**派生摘要**进内核私有记忆，供 AI 上下文（真相仍在 Sin90 SQLite） | `_a24/memory/private/{remember,recall,recent}` |
| approval | （预留）需要内核级人工审批的动作 | `_a24/approval/*` |
| models | M5 AI 推理（内核强制 LocalOnly，除非 manifest 声明 remote_allowed） | `_a24/model/complete` |

## 不可破边界

1. **SQLite 是真相，事件只追加**；任何新实体都要产事件（DESIGN §2 #12）。
2. **写内核的副作用只经 `sin90_outbox` 幂等对账**，不跨库两阶段提交；对账器可以重复执行而结果不变。
3. **AI 不直写**：`ai/` 只能 `submit_proposal`；提议的 accept 只认人类 key（#1）。结构测试钉住。
4. **`/_a24/*` 路由只在挂载模式存在**：它的可信性来自内核代理剥掉客户端的 `X-A24-*` 头并拒绝客户端访问保留路径；standalone 模式没有这层，所以不注册。
   这只防「经内核 HTTP 入口的外部客户端」，不防同 UID 本地进程直连 Sin90 的 socket（Agent24 SPEC-ME3 §0）。fired 是**至少一次**投递，按 `fire_id` 幂等处理。
5. **回调通道多路复用**（T3.2.0 起）：并发调用不互相阻塞；重连后 Offer 原子更新；内核客户端不依附于 EventSink。
6. **内核里用户的暂停高于 Sin90 的对账意愿**：`user_suspended` 的定时 Sin90 不去改。
7. **只声明真正用到的能力**；代码按「句柄可能不在」写（`Offer.provides` 没给就是 `None`）。
8. **adapter 是唯一懂线协议的地方**，业务代码只见类型化客户端（为 MS 换 SDK 留缝）。
