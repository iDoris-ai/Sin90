# 状态：Sin90 挡在 Agent24 的哪一刀上

> 本文写的是**可核实的事实**，不是路线图口号。每条都给了你自己去核的办法。

## 今天的硬事实

Agent24 的内核在挂载时会拒绝任何 `impl_kind: out_of_process` 的包：

```
rust/apps/agent24d/src/domain.rs:727    if !manifest.is_mountable_in_process()
rust/crates/agent24-domain/src/lib.rs   is_mountable_in_process() 只对 InProcessCrate 返回 true
```

源码注释原文：*"ME-3's transport does not exist yet, so the in-process mounter MUST refuse it rather than half-mount a config it cannot honor."*

**自己核**：在 Agent24 仓库里 `grep -n "is_mountable_in_process" -A6 rust/crates/agent24-domain/src/lib.rs`。

Agent24 的 ME-3a（已合）交付的是**发现 + 安装**：`agent24 os install <目录>` 能把包放进 `~/.agent24/packages/`，daemon 下次启动会扫到。**扫到 ≠ 装得上。**

## Sin90 要等哪几刀

| Agent24 的刀 | 状态 | Sin90 要吗 |
|---|---|---|
| ME-3a 发现与安装 | ✅ 已合 | — |
| ME-3b-2a 版本协商 | ✅ 已合（🟢 库层可用，无生产调用方） | ✔ |
| ME-3b-1 framing | 🔵 PR #164 复审中 | ✔ |
| ME-3b-2b `initialize` 线格式 | 未开工 | ✔ |
| ME-3b-3 spawn + 进程监督 | 未开工 | ✔ |
| ME-3b-4 受约束代理 | 未开工 | ✔ |
| ME-3b-5 两阶段热 disable | 未开工 | ✔ |
| ME-3c 回调通道其余部分 | 未开工 | ✔ |
| **ME-3d 记忆回调** | 未开工 | **✘ 不要** |
| ME-3e 事件 + 审批 | 未开工 | ✔（只要 events 那半） |
| ME-3f 仓外包端到端 | 未开工 | ✔ **这条是验收** |
| ME-3g 启用路径准入 | 未开工 | ✔ |

**为什么 Sin90 不需要 ME-3d**：`domain-os.yml` 里 `kernel_capabilities: [events]`，数据放在自己的 `data_dir`。内核记忆那一刀（租约、配额、分区键）是全 ME-3 里否定用例最多的一刀，Sin90 整条不碰。

**ME-3f 是唯一算数的判据**：先构建 daemon；之后生成并安装一个**仓库之外**的包，不改源码、不重新构建，重启后完成挂载 → 路由代理 → 事件转发。没跑通 3f，「支持第三方 OS」只是一句声称。

## 还有一件 Agent24 的 SPEC 里今天没有的

「第三方照模板写自己的」要成立，必须有**模块侧 SDK** 或一份**足够完整的 wire 文档 + 参考实现**。Agent24 SPEC §8 今天只交付一个 mock provider 包。这件事没定之前，本仓的 `src/handshake.rs` 只能是桩。

跟踪：Agent24 `docs/agent/PLAN-OOP-OS-AND-BACKLOG.md` 的 T0.2。
