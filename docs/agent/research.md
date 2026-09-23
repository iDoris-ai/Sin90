# Sin90 立项依据（摘要）

> 完整讨论：[`../LIFEOS-DESIGN-INPUT.md`](../LIFEOS-DESIGN-INPUT.md)；逐条裁决：[`../DESIGN-LIFEOS.md`](../DESIGN-LIFEOS.md) §2、§9（参考项目表）。

- **为什么是进程外包**：用户 2026-09-10 裁决 Sin90/Cos72 与内核分开发布（Agent24 T11 已执行）；个人 OS 的迭代速度不应被内核发版节奏绑住。
- **为什么调度交给内核**：DESIGN §2 #9 —— 自建 Reminder = 重写内核已有的调度器；Sin90 只声明 cron，经 outbox 幂等落到内核。
- **为什么 AI 只产提议**：「AI 不直写」是个人数据主权的底线（MISSION：数字主权）；#1 已把 accept 收紧到只认人类 key。
- **为什么默认本地模型**：个人数据不出机器是默认值，远端是显式选择（Agent24 ME-4 裁决 D2）。
- **参考**：quanru/obsidian-lifeos（Quick Capture、周期复盘）等，取舍见 DESIGN §9。
- **License**：Apache-2.0（与 Agent24 一致）；中文法务文档缺口（`LICENSE-zh.md` 等）登记在 followups，不阻塞开发。
