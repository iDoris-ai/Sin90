# Sin90 实时状态 — progress

> 「此刻仓库真实发生了什么」。由 `pilot run` 每一步更新。更新时间：2026-09-23。

## 当前聚焦

- 本轮 = Agent24 **ME-4** 的 Sin90 半边：M0' 清账 → M3 Routine & Rhythm → M4 Review & Markdown → M5 AI v1 → MS 迁 SDK。
- 流程改为 **PR + clestons 评审**（用户 2026-09-23 裁决）；此前 M0–M2 是本地 `--no-ff` 合并，没有 PR。

## 下一个 READY

1. T0.1 合并 #2/#3/#4（均已 APPROVED、MERGEABLE）。
2. T0.2 CI、T0.4 Codex 补审（依赖 T0.1）。
3. T3.1.1 / T3.4.1 可在 T0.1 之后直接开工（不依赖内核）；T3.2.1 等 Agent24 `ME4-1.1.1` 冻结线格式。

## 2026-09-23 夜 → 09-24 凌晨：无人值守一夜的战报

当晚只开 PR、不盯状态（用户指示），**没有合并**；#2、#3 在当晚早些时候已合并（exact-head approve）。评审全部为全新上下文 Opus 子代理（Codex 额度 09-29 恢复），记 Agent24 `ME4-CODEX-DEBT-4`。

**已开 PR**（stacked，按合并顺序）：
- #4（rebase 后待 clestons 重审）
- #5 pilot 规划层（本文件所在分支）· #6 CI · #7 陈旧文档 · #8 Rhythm 路由（T3.4.1）
- Routine 线：#9 模型层 → #10 create/get/list → #11 update/transition → #12 路由（T3.1.2）→ #13 outbox 写入（T3.3.1）→ T3.2.2 fired 路由（拆分中）
- Transport 线：#14 帧 → #15 transport 原语 → #16 KernelClients → #17 EventSink → #18 错误闭集 → #19 调度客户端 → #20 记忆/审批客户端（T3.2.0 / T3.2.1）

**T3.3.2（对账器）暂缓**：它同时依赖 Routine 线（outbox）与 Transport 线（调度客户端），两条 stacked 线在合并进 main 之前没有共同的基点；等两线合并后从 main 开工，避免造一个跨线的临时集成分支。
**评审改进的规划**：回调连接终身持有、断了就退出不重连（内核合约）；cron 星期只收英文缩写、日/星期不同时受限；迁移编号不留空洞。

## 阻塞项（BLOCKED）

- 无。M3 的内核半边（调度回调）由 Agent24 `ME4-M1` 提供，属依赖不属阻塞。

## 待用户拍板（T5.0.1 AI v1 设计提出；设计里暂用保守占位，不影响 M3/M4 推进）

设计草稿：`docs/t5.0.1-ai-v1-design` 分支 DESIGN-LIFEOS §11.11。第 1 轮对抗评审后，Q1/Q2/Q5 已由规格与技术约束决定，不再需要拍板：
- ~~Q1~~ 正式包**不声明** `remote_allowed`——验收原文「不开远端时只用本地模型」排除了声明它（一旦声明，内核对 Sin90 的所有推理都不再保证只走本地）。将来要 executive，需要 Agent24 先加「逐次只能收窄的隐私字段」（Agent24 FU-77）。
- ~~Q2~~ executive 开关默认关（规格已定）。
- ~~Q5~~ reflex 产出记 `source = rule`（tasks.md T5.5.1 已含，只需同步文字）。

**仍需你定**：
- **Q3** AI 多久触发、要不要自动触发（capture 后 / 复盘 Routine 到点 / 周进入 planning / 定时）？（占位：只手动触发）
- **Q4** 没有合适的 Direction 时，classify 要不要提议新建一个？（占位：不要；v1 技术上也做不到批内前向引用）
- **Q6** M5 要不要加 `POST /proposals/{id}/reject`？（没有拒绝入口时挂起提议会无界增长，评审建议 M5 后尽快做）
- **Q7** 已写过的复盘草稿，summarize 能否提议替换？注意这是**整体替换**（模型看不到现有正文，accept 后人写的字会丢）。（占位：只在正文为空或只有程序块时提议）
- **Q8** classify 低置信度时要不要仍出提议？（技术输入：本地小模型自报置信度校准差，阈值建议用冒烟数据定）

## 需要用户做的事

- main 无保护：请开 ruleset（1 个审批 + dismiss stale）。在此之前合并用 `gh pr merge --squash`（前提 clestons APPROVED）。

## 最近完成

- 2026-09-23 #1 `accept_proposal` 只认人类 key（`8eb2d50`）。
- 2026-09-20 真实 Agent24 挂载验证（`ab66b37`）；M2（`8056ade`）；M1（`4032e82`）；M0（`0d66f24`）。
