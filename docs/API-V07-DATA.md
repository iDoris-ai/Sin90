# v0.7 S1 数据接口

Agent24 挂载前缀为 `/api/v1/sin90`，下文省略前缀。所有 POST 均要求现有 human actor key（`x-sin90-actor-key`），automation key 返回 403；两个 GET 无需钥匙。错误沿用 `{ "error": { "code", "message" } }`。

| 方法与路径 | 请求 JSON | 成功响应 JSON（200） |
|---|---|---|
| GET `/data/status` | 无 | `{template: null \| {id,lang,loaded_at,title,guide}, backups:[{name,at,bytes}], counts:{areas,directions,tasks,plans,routines}}` |
| GET `/templates` | 无 | `[{id:"graduate",langs:["zh","en","th"],title:"示范：刚毕业的第一年"}]` |
| POST `/data/export` | 无 | `{format:"sin90-backup",schema:18,exported_at,tables:{"sin90_表名":[{列名:值}]}}` |
| POST `/data/import` | `{backup: 导出响应}` | `{ok:true}` |
| POST `/data/reset` | `{}` 或 `{template:"graduate",lang:"zh"}` | `{ok:true}` |
| POST `/data/restore-backup` | `{name:"auto-….json"}` | `{ok:true}` |

`lang` 支持 `zh`、`en`、`th`；手动 reset 省略时为 `zh`。首次启动优先读取非空 `LC_ALL`，其次 `LANG`：`zh*` → zh，`th*` → th，其余 → en；环境变量缺失或为空时默认 zh（按 #93 评审补充，适配 GUI/launchd）。三种模板均通过 `include_str!` 编入二进制。

模板元数据中的 `guide` 可直接用于示范横幅；数据库保存 `template.id`、`template.lang`、`template.loaded_at` 三条标量设置。空白 reset 和 import 都会在数据替换事务里设置 `template.autoload_done=true`，后续重启保持用户当前数据。

counts 包含系统种子：示范加载后为 areas=6、directions=7（含待定）、tasks=7（今日3 + 收件箱4）、plans=30、routines=7（示范6 + 系统复盘1）。计划为 active、source=ai；年/季/月与本地日历对应；今日任务为 planned，绑定当前 ISO 周。依照方案补充约束，block 不增加周列；加载时创建当前周和4个 planned 块，共315分钟。

export 包含所有 `sin90_*` 表，不包含 sqlx 迁移表；schema 来自实际已应用迁移。普通单元格为 JSON null / string / number，BLOB 使用 `{blob_hex:"…"}`。schema 必须与当前版本相同，较新、较旧、未知表/列、无效计划、无效 cron/tz 和外键断裂均返回400；失败时事务整体回滚。

import 与 reset 必须先成功写入 `<data_dir>/backups/`；没有持久化 data_dir 的开发模式，其 reset、import、restore 返回409。备份列表按新到旧排序，只保留最近10份 `auto-*.json`，用户另存的文件不会删除。restore 的 name 仅接受 ASCII 字母、数字、点、横线、下划线组成的 `.json` 文件名，拒绝路径逃逸和符号链接。

按方案 #93 的恢复例外：

- 业务行与历史事件保留原值，事件原始 seq 一起恢复。
- 不清空 attention watermark 单例；在同一事务里从事件重建 attention。
- 不导入 outbox；active Routine 重建 scheduler upsert，paused 重建 enabled=false 的 upsert，被移除的 Routine 重建 delete。
- 设置只导入 `review`、`progress.streak_weeks`、`template.*`，不恢复 `ai.*`；确保系统待定方向、复盘 Routine、review 设置和初始化标记存在。

因此逐行恢复验收比较业务表与事件；outbox、被过滤的设置和过期的 attention 是明确的恢复例外。progress 从原始事件重建，并保持恢复前后一致。新建计划的 expected 和周达标序列从期间起点与创建日期中较晚的一天开始；期间内历史完成量依旧计入 actual。

数据替换完成会发出 `data.imported`、`data.reset` 或 `data.restored` 通知，payload 为 `{}`。前端收到后重新获取数据和 `/data/status`。
