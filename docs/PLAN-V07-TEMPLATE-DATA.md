# Sin90 v0.7：示范数据、备份恢复、AI 拆分

日期：2026-11-19 · 状态：草案（一轮评审后冻结）
来源：jason 11-19。他认为方向稿（https://claude.ai/artifact/W4KYLAAN62CqFwztq5u4BA）设计得不错，但普通用户拿到一个空系统，不知道怎么用。

## 0. 一句话

装上 Sin90 就带着一套**示范人生系统**：一个 23 岁刚毕业的年轻人，五个人生系统加上「探索与创造」，每个系统写好现在和理想、方向、三年目标，今年的目标拆到季度和本月，每周有例行节奏，今天有三件必做。用户可以在示范上直接改成自己的，也可以清空重来。任何清空或恢复操作之前都会自动备份，备份随时能导出、导入。

## 1. 产品规则（jason 已定）

1. **默认加载示范**。新装 Sin90、库里还没有用户数据时，自动加载示范。已经有数据的用户不会被自动覆盖。
2. **示范要说清楚自己是示范**。数据里带一段说明（`guide`），桌面上用一条横幅告诉用户「这是示范，可以改，也可以清空」。
3. **可以清理，也可以恢复**。
   - 清空：可以清成空白，也可以重新加载示范。
   - 恢复：从自动备份或用户导出的文件恢复。
4. **一个人的 Sin90 数据可以整体备份和恢复**：导出成一个文件，再从文件导入。
5. 示范内容由主会话以「全能导师」的视角撰写，见 `templates/graduate.zh.json`。英文和泰文版由翻译生成，并经过人工核对。

## 2. 示范内容（`templates/graduate.<lang>.json`）

人物：小周，23 岁，毕业半年，做第一份工作，一个人住在一座大城市。

| 系统 | 现在 → 理想 | 三年目标 |
|---|---|---|
| 工作 | 靠加班补能力，没有作品 → 成为团队里让人放心交付的人 | 能独立负责一个完整项目，有 3 件作品 |
| 健康 | 久坐、吃外卖、晚睡 → 睡眠规律，坚持力量训练，饮食以原型食物为主 | 23:30 睡、7:30 起；深蹲能做到自身体重；跑完 10 公里 |
| 学习 | 刷短视频，读过就忘 → 建立 AI、哲学、历史、艺术的知识地图 | 36 本书、36 篇笔记 |
| 关系 | 朋友散了，和家人联系少 → 有 3–5 个能深谈的人 | 5 个能深谈的朋友，每年认识 10 个新朋友 |
| 财富 | 月光、不记账 → 有 6 个月应急金，开始定投 | 存下 20% 收入，定投坚持 24 个月 |
| 探索与创造 | 下班只剩刷手机 → 有一个长期的个人项目 | 12 件作品，公开 6 件 |

健康部分写进了具体做法：
- 吃饭先吃蔬菜，再吃蛋白质，最后吃主食。
- 蛋白质每天按体重算，每公斤 1.2–1.6 克。
- 力量训练每周 2–3 次，练深蹲、臀桥、俯卧撑、划船。
- 有氧运动每周 150 分钟。
- 每年体检一次。

学习部分按 AI、哲学、历史、艺术四个方向各给了入门书目。

**相对时间**。模板里不写具体年份，加载时按当天的日期展开：
- 三年目标的起始年 `goal_from_year` 取今年。
- `years[0..2]` 对应今年、明年、后年。
- `quarter` 对应当前季度，`month` 对应当前月。
- `week_blocks` 和 `today` 放进当前 ISO 周。

## 3. 底层改动

### S1 · 数据：备份、恢复、清空、示范（先做）

- `GET /data/status` 返回当前数据的状态，结构如下：

  ```
  { template: {id, lang, loaded_at} | null,
    backups: [{name, at, bytes}],
    counts: {areas, directions, tasks, plans, routines} }
  ```

- `GET /templates`：列出内置模板，返回 `[{id, langs, title}]`。
- `POST /data/export`（需要 human）：返回一个 JSON，内容包括格式名 `sin90-backup`、`schema`（最新迁移号）、`exported_at`，以及 `tables`。`tables` 是每张 `sin90_*` 表的全部行，不包括 sqlx 的迁移记录表。
- `POST /data/import {backup}`（需要 human）：
  1. 检查格式和 `schema`。备份的 `schema` 比当前版本新时拒绝。
  2. 先把当前数据**自动备份**到 `<data_dir>/backups/auto-<时间>.json`，只保留最近 10 份。
  3. 在一个事务里清空所有 `sin90_*` 表，再写入备份里的行。
  4. 重建 attention 物化视图。
  5. 对导入后处于 active 状态的 Routine，给 kernel scheduler 补发 upsert；导入前存在、导入后已经没有的 Routine，补发 delete。
- `POST /data/reset {template?, lang?}`（需要 human）：
  1. 先自动备份。
  2. 清空所有数据。
  3. 补回系统种子：待定方向、系统复盘 Routine、复盘设置。
  4. 请求里带了 `template` 时，再加载对应的模板。
- `POST /data/restore-backup {name}`（需要 human）：从 `<data_dir>/backups/` 里的某一份备份恢复，走的流程和 import 相同。
- **首次自动加载**：启动时满足以下全部条件就加载 `graduate`，然后在设置里写入 `template.autoload_done=true`：
  - 库里没有用户数据：没有 area，除待定方向外没有 direction，没有 task；
  - `template.autoload_done` 尚未设置。

  加载哪种语言看 `LANG`：以 `zh` 开头用 zh，以 `th` 开头用 th，其余用 en。

  已经有数据的用户不会被自动加载，可以在桌面上手动选择「加载示范」。手动加载会先备份，再清空，然后加载。
- **模板加载器**：
  - 走现有的写入路径（`create_area`、`update_area`、`create_direction`、`update_direction`、period plan、routine、task、week、block、capture），这样产生的事件和表结构与手动操作完全一致。
  - 计划的 `source=ai`，以便后续 AI 拆分时可以细化；用户改过之后变成 `user`。
  - quarter 计划的 parent 指向 year[0]，month 计划的 parent 指向 quarter。
  - routine 的 `tz` 用 `system_tz_name`。
  - 最后在设置里记下 `template = {id, lang, loaded_at}`。
- **时区重新同步**（v0.6.1 的待办）：启动时如果系统复盘 Routine 的 `tz` 与 `system_tz_name` 不一致，就更新它，并补发一次 scheduler upsert。

#### S1 实现约束（#93 评审补充，必须照做）

1. **自动加载只做一次**
   - reset 和 import 清库后，在**同一个事务**里写入 `template.autoload_done=true`。
   - 自动加载只在「`autoload_done` 未设置，并且是全新库」时触发。这样用户清成空白后重启，示范不会又冒出来。
   - **原子加载靠「暂存库 + 一次替换」实现**，不是把现有写入函数套进一个外层事务。原因是 `create_area`、`create_direction` 等函数各自 `BEGIN IMMEDIATE`，SQLite 只有一个写者，外层事务会把它们卡到 `busy_timeout` 再失败。
     1. 新建一个内存库，先写入 `autoload_done`，再走现有写入路径把模板加载进去。事件和表结构与手动操作完全一致。
     2. 把暂存库导出成备份。
     3. 用 import 同一个「清空 + 写入」事务，一次性替换目标库。
     - 中途任何一步失败，目标库都不受影响，不会留下半套示范。失败后不会自动重试，等用户手动「加载示范」。
   - **写入顺序**：import / reset 的事务里，`template.autoload_done=true` 必须在 settings 白名单导入**之后**写。否则备份里带的 `template.*` 旧值会把它盖掉。
2. **保留系统单例**
   - `sin90_attention_watermark` 不清空。reset 和 import 之后只执行 `attention_rebuild`。
   - 「补回系统种子」的清单：待定方向、系统复盘 Routine、`review` 设置、水位线行 `(1,0)`。
3. **import 不能原样恢复的表**
   - `sin90_outbox` 不导入。清空之后，对每条 Routine 重新生成：
     - active → 生成 upsert
     - paused → 生成 upsert，`enabled=false`
     - 导入前存在、导入后已没有的 → 生成 delete
   - `sin90_settings` 只按白名单导入：`review`、`progress.streak_weeks`、`template.*`。`ai.*` 永远不导入。导入后要保证 `review` 行存在。
   - 事件表连同原始 `seq` 一起恢复，否则「导入后 progress 一致」不成立。
4. **外键**
   - 事务内设 `PRAGMA defer_foreign_keys=ON`，提交前执行 `PRAGMA foreign_key_check`，有违规就回滚并返回 400。
   - 原因：`carried_from`、`parent_task_id`、`parent_id` 都是自引用，插入顺序不对会撞外键。
5. **校验不能绕过**：import 对每条 Routine 重跑 `validate_cron` 和 `validate_tz`，对每条计划重跑 `validate_plan` 和 `validate_parent`，不合格返回 400。
6. **schema 必须完全一致**：备份的 `schema` 必须等于当前迁移号，新的旧的都返回 400，并提示「用对应版本导出」。
7. **备份文件名与路径**
   - `restore-backup` 的 `name` 只允许匹配 `^[A-Za-z0-9._-]+\.json$`，解析后的路径必须落在 `backups/` 内，做法参照 `finalize_review` 的防逃逸。
   - `data_dir` 为空（独立运行时没带 `--data-dir`）时，reset、import、restore 一律返回 409，不允许在没有备份的情况下继续。
   - 「最多保留 10 份」只统计 `auto-*` 文件，用户自己导出的文件不删。
8. **加载器细节**
   - 方向的 `target_window` 用当年（`YYYY`）。创建后通过 `update_direction` 把状态设为 active，并写入 `goal_3y` 和 `goal_from_year`。
   - 任务的 `kind` 取模板里的值，没写就用 `other`。
   - 「本周的块」的做法：建出当前周（`sin90_weeks`），再建若干 planned 状态的 block。block 本身没有周列。
   - graduate 模板和现有的 `/packs/install`（五个系统的种子）**并存**：packs 只建系统和方向，不变；graduate 是一套完整示范。
9. **语言**：没有 `LANG` 或 `LC_ALL` 时默认用 zh，因为 macOS 的 GUI 和 launchd 进程通常不带 `LANG`。`POST /data/reset` 可以显式传 `lang`，桌面端会传。
10. **中途创建的计划，进度从创建日开始算（改 L3 的 progress）**
    - `expected` 和周达标序列的起点取「期间起点」和「计划创建日」中较晚的那个。创建之前的周不算未达标。
    - 原因：11 月加载的年度计划，或者任何时候新建的计划，不应该一创建就显示落后。
    - **这是对 L3 进度语义的有意改动**，需要同步修改现有测试。那些用例先建过去期间的计划，再断言历史周未达标（如 `progress.rs` 的 `one_missed_week_stays_local…`、`src/http/tests.rs` 的连续三周用例）。改法是把这些用例中计划的 `created_at` 设到期间起点。
    - 导入备份时保留计划原来的 `created_at`。进度从事件重放，payload 里带着 `created_at`，所以导入前后的 progress 一致。
    - 手工补录过去期间的计划时，创建之前的周不再算未达标。这是可以接受的：补录的人知道那几周没在跟踪，系统不应该据此判定落后。
11. **`template` 设置拆成三行**：`template.id`、`template.lang`、`template.loaded_at`，符合 0009 中「value 是标量」的约定。

### S2 · AI 拆分（v0.6 方案里的 L2b）

- `POST /ai/decompose {direction_id, down_to: "month"}`：
  - 输入是方向、三年目标、已有计划和近期实际进度。
  - 产出一个 `UpsertPeriodPlans` 提案，覆盖今年的年度计划，以及当前的半年、季度、月度计划。
  - 走现有 ladder：优先用本机模型。没有模型时按规则兜底：上一层的量化目标按时间均分，文字写成「〈上一层目标〉· 〈期间〉部分」。
  - 提案需要人采纳才生效，并沿用 L2a 的规则：跳过 `source=user` 的行。

## 4. 验收

| 片 | 关键验收 |
|---|---|
| S1 | 新库启动后自动载入示范，有 6 个系统、各自的方向、年度×3、季度、月度计划，6 条 routine，今天 3 件，收件箱 4 条；再启动一次不会重复加载。已有数据的库不自动加载。导出后清空再导入，所有表的行与导出前逐行一致，progress 结果也一致。reset 前一定先生成自动备份，最多保留 10 份。备份的 schema 比当前新时，import 返回 400。中、英、泰三份模板都能加载。时区重新同步生效。 |
| S2 | 没有模型时按规则兜底，能从年度一路拆到月度；提案采纳后才写入；`source=user` 的行被跳过。 |

## 5. Agent24 侧（另开 PR）

- **A1 编辑**：Sin90 页面上可以编辑以下内容：
  - 系统的「现在 / 理想」
  - 方向的三年目标
  - 各级阶段计划（新增、修改、放弃）
  - 自动复盘设置（`PUT /settings/review`）
  - 方向卡片上加一个「AI 拆分到本月」按钮（S2）
- **A2 数据面板**：
  - 示范横幅：显示 `guide` 文案，提供「清空，从我自己开始」和「重新加载示范」。
  - 数据页：导出备份（主进程弹出保存对话框）、从文件导入、从自动备份恢复（列表）。

所有写操作都走主进程的 `sin90:write`，需要人类钥匙和委托。

## 6. 待办（这次不做）

- 多套模板，比如学生、职场中期、退休。
- 把示范数据和用户数据混在一起保存。本版的做法是清空后整体替换，不做逐条合并。
- 云端同步备份。
