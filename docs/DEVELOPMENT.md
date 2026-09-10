# 开发建议

按「现在能定的」和「必须等的」分开。混着写，等于给自己安排返工。

---

## 一、现在就能写，且形状不会变

### 1. 清单：四处必须一致

`domain-os.yml` 里 `name` 是身份的唯一来源，另外三处由它派生：

```yaml
name: sin90
route_namespace: /api/v1/sin90      # 必须恰好是 /api/v1/<name>
event_module: sin90                 # 必须恰好是 <name>
data_dir: ~/.agent24/os/sin90/      # 必须恰好是 ~/.agent24/os/<name>/
```

写成别的会被校验拒绝 —— 这是故意的：清单不可能悄悄把模块指向另一个 OS 的路由或数据。

**模块名的规则**（Agent24 `is_valid_module_name`）：ASCII 小写字母数字加 `-`/`_`，首字符必须是字母或数字，有长度上限，且不能是保留名。比事件 schema 的规则更严，因为这个名字同时是 **URL 段**和**目录名**；限制成 ASCII 还消掉了 Unicode 归一化的别名问题（`é` 与 `e`+U+0301 会落到同一个目录）。

### 2. 只声明真正用到的能力

```yaml
kernel_capabilities: [events]
```

多要没有任何好处：**内核取交集**，而且会在 `agent24 os` 的输出里把模块描述错。

关键：Agent24 里「授予」不等于「拿得到」。真正的权限是**持有内核造的句柄** —— 没被授予 events，`KernelCtx::events()` 返回 `None`，没有对象可调。所以你的代码要按「句柄可能不在」来写，而不是按清单里写了什么来写。

### 3. HTTP handler —— 你主要的工作在这里

内核的受约束代理是**透明转发**：`/api/v1/sin90/*` 原样转给你的进程。所以你写的就是普通 HTTP 服务，路由**用相对路径**（`/directions`，不是 `/api/v1/sin90/directions`）—— 命名空间由内核加。

Sin90 今天在 Agent24 内核里的七条路由，可以当形状参考：

```
POST/GET  /directions
POST/GET  /schedule-blocks
PATCH     /schedule-blocks/{id}
POST/GET  /proposals
GET       /proposals/{id}
POST      /proposals/{id}/accept
```

### 4. 自己的存储

Sin90 不用内核记忆，数据在自己的 `data_dir`。**这是一个降低耦合的选择，不是将就** —— 它让 Sin90 整条绕开 ME-3d（内核记忆回调，全 ME-3 里最难的一刀）。

你的模块如果也能自己管数据，建议照做：能早很多拿到「可独立装载」。

---

## 二、必须等的

### 握手与回调通道

`src/handshake.rs` 今天是桩。Agent24 的 `initialize` 线格式（ME-3b-2b）还没写；已经落地的只有**版本协商**（纯函数）与 **framing**（NDJSON 单帧 + 单行上限）。

已经能确定的两条约束，写代码时可以先按它们来：

- **单帧上限 1 MiB，超限断连、不降级。** 这个数是在消费方存在之前定的，Agent24 记了一条挂账（FU-42）要在 `initialize` 定稿前用真实 wire shape 重新钉。**别把大 payload 塞进单帧。**
- **版本区间协商，不报区间 = 不兼容。** 沉默不会被当成同意。你的 `initialize` 必须报 `[min, max]`。

### 不要现在做的事

- 不要照猜的协议写握手代码 —— 它会变。
- 不要把 `impl_kind` 改成 `out_of_process` —— 今天挂载时被硬拒，你只会得到一个说不清原因的失败。

---

## 三、两条从 Agent24 借来的工程习惯

这两条不是风格，是这个项目反复被自己咬到之后留下的。

### 判据本身要先被验过

一条测试「绿」有两种原因：被测的东西是对的，或者**判据根本不会响**。所以每写一条判据，配一次反向验证：**故意把实现改坏，那条测试必须变红**。改坏了还是绿，说明你测的不是你以为的东西。

同样地，用 grep/计数当判据时要带**正对照** —— 一个恒为 0 的计数和一个真的是 0 的计数，看起来一模一样。

### 措辞不能比机制强

注释和文档里写「保证 X」之前，问一句：**什么东西会在 X 不成立时响？** 没有，就把话写弱到与机制相符。Agent24 里被返工最多的不是代码，是**比代码承诺得多的句子**。

---

## 四、跟踪

- 状态与依赖：本仓 [`docs/STATUS.md`](STATUS.md)
- Agent24 侧的任务分解：`docs/agent/PLAN-OOP-OS-AND-BACKLOG.md`
- 设计文档：Agent24 `docs/specs/SPEC-ME3-OUT-OF-PROCESS.md`
