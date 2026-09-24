# 状态：Sin90 现在能做什么，不能做什么

> 本文写的是**可核实的事实**，不是路线图口号。每条都给了你自己去核的办法。
> 设计与里程碑见 [`DESIGN-LIFEOS.md`](DESIGN-LIFEOS.md)，本文不复述它。
> 最后核实：2026-09-20（实读 Agent24 main）

## 阻塞已解除（2026-09-20）

**旧版本的本文写着"作为独立包被 Agent24 装载 ❌ 要等 Agent24 v0.5.0"——这个阻塞条件已经不成立。**
Agent24 的 ME-3 专项（进程外领域 OS）已于 2026-09-20 整体收口：T9/ME-3f 仓外包端到端黑盒验收合入 main（commit `164120b`，PR #262；状态同步 `9ab5b6e` / PR #341）。

**自己核**：在 Agent24 仓库里跑 `cargo test -p agent24d --test me3f_blackbox`，或读
`rust/apps/agent24d/tests/me3f_blackbox.rs` 的模块文档——它描述的是**先构建 daemon、再在仓库之外生成并安装一个包、不改源码不重新构建、重启后挂载 → 路由代理 → 事件转发 → 记忆读写 → 审批往返全绿**。

## 今天的状态表

| | 状态 |
|---|---|
| 业务半边（HTTP 路由 + 自己的存储） | ✅ 能写，形状已定 |
| 作为 `out_of_process_provider` 被 Agent24 装载 | ✅ **通道已通** —— 内核不再硬拒 out-of-process 包 |
| 握手 / 回调通道（`initialize`、事件上报） | ✅ 已在 `src/adapter_agent24/mod.rs` 自己实现；⚠️ **仍没有 Rust SDK**（见下） |
| 本仓库的 `src/*` | ✅ 不再是桩 —— `core/store/http/adapter_agent24` 四层已齐备，M0/M1/M2 已合入（见 `DESIGN-LIFEOS.md` 进度表） |

## 进程外模块的对接契约（实测形状，2026-09-20）

内核 spawn 子进程时通过环境变量交付四样东西：

| 环境变量 | 含义 |
|---|---|
| `A24_DATA_DIR` | 模块自己的数据目录（`~/.agent24/os/<name>/`） |
| `A24_CALLBACK_SOCK` | 回调通道的 Unix socket（NDJSON / JSON-RPC） |
| `A24_HANDSHAKE_TOKEN` | `initialize` 时必须回报的令牌 |
| `A24_LISTEN_FD` | 内核**已 bind 好**的 HTTP listener fd，模块直接 `accept` |

`initialize` 请求体：
```json
{"protocol_versions":{"min":1,"max":1000},"module":"<name>",
 "manifest_digest":"sha256:<domain-os.yml 的摘要>",
 "auth_token":"<A24_HANDSHAKE_TOKEN>",
 "capabilities":["events"]}
```

manifest 的 `impl_kind: out_of_process_provider` 必须**同时**带 `spawn: {command, args}`；两个方向都校验（少一个或多一个都被拒）。`spawn.command` 不许绝对路径、不许含 `..`。

**自己核**：`grep -n "A24_LISTEN_FD\|A24_CALLBACK_SOCK" rust/apps/agent24d/tests/me3f_blackbox.rs`；
manifest 双向规则在 `rust/crates/agent24-domain/src/lib.rs` 的 `match (raw.impl_kind, raw.spawn.as_ref())`。

## 还剩一件 Agent24 没给的

**`agent24-os-sdk` 这个 crate 今天不存在。** Agent24 的 T13（模块侧 SDK）与 T14（wire 文档 + 非 Rust 参考实现）都未开工——`ls rust/crates | grep os-sdk` 空。

本仓已经选择**自己实现**（而不是等 SDK）：`src/adapter_agent24/mod.rs`（约 450 行）已经实现了
`initialize` 握手与事件上报，返工范围也如预期地钉死在这一个目录里。

跟踪：Agent24 `docs/agent/PLAN-OOP-OS-AND-BACKLOG.md` 的 T13 / T14。

## 下一刀

M0/M1/M2 已合入 main。**下一刀是 M3（Routine & Rhythm）**——进度与阻塞见
[`DESIGN-LIFEOS.md`](DESIGN-LIFEOS.md) 顶部的进度表。
</content>
