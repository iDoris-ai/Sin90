# Sin90

个人助理的领域 OS —— 方向（Direction）、节奏（Rhythm）、任务（Task）三层，跑在 [Agent24](https://github.com/iDoris-ai/Agent24) 上。

**同时是「怎么写一个 Agent24 领域 OS」的参考实现。** 另一个样例是 Cos72（社区 OS）。

---

## 现在能做什么，不能做什么

| | 状态 |
|---|---|
| 业务半边（HTTP 路由 + 自己的存储） | ✅ **现在就能写**，形状不会因 ME-3 改变 |
| 作为独立包被 Agent24 装载 | ❌ **要等 Agent24 v0.5.0**（见 `docs/STATUS.md`） |
| 握手 / 回调通道（`initialize`、事件上报） | ❌ 协议未定稿，本仓只有桩 |

**为什么业务半边现在就能写**：Agent24 的受约束代理是**透明转发** —— 内核把 `/api/v1/sin90/*` 原样代理给模块进程，模块只要在自己的命名空间下提供 HTTP 服务。这一层与握手层是两件事，握手怎么定，都不改你的 handler 长什么样。

**为什么握手半边要等**：`initialize` 的线格式是 Agent24 的 ME-3b-2b，今天还没写。现在照一个不存在的协议写代码，等于给自己安排一次返工。

---

## 快速开始

```bash
cp -r . ~/my-domain-os && cd ~/my-domain-os
$EDITOR domain-os.yml     # 改 name / route_namespace / event_module / data_dir —— 四处必须一致
cargo test
```

写你自己的 OS：读 [`docs/DEVELOPMENT.md`](docs/DEVELOPMENT.md)，它按「现在能定的」和「必须等的」分开讲。

---

## 目录

```
domain-os.yml        清单 —— 模块身份的唯一来源
src/routes.rs        业务半边：HTTP handler。★ 你主要改这里
src/store.rs         自己的持久化（Sin90 不用内核记忆）
src/handshake.rs     握手半边：桩。v0.5.0 由 Agent24 的 SDK 填
docs/STATUS.md       它挡在 Agent24 的哪一刀上，以及怎么自己核实
docs/DEVELOPMENT.md  开发建议
```

## 许可

Apache-2.0，与 Agent24 一致。数字公共物品：开源、免费、无许可。
