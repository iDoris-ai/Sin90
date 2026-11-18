# L3 进度接口

`GET /progress?direction_id=<id>&at=YYYY-MM-DD` 返回 `{ "progress": [...] }`。
`direction_id` 必填；`at` 省略时取本机时区的今天。每行包含 `plan_id`、
`actual`、`expected`、`status`、`weeks` 和 `miss_streak`。
不存在计划的方向返回空数组。此接口只计算，不写入计划状态。

计划和 routine 使用事件日志中最新的 created/updated 快照；`at` 是完成统计的
截止日，包含当天，不是配置版本的时间旅行。任务方向按其事件依次回放；块的方向、
routine 和分钟数取完成事件。完成时刻转换成本机时区后，按日历日归属，忽略块的周归属。
计划、任务、块、routine 可变表的直接修改不改变统计。

`count` 有 routine 时统计关联块转为 completed 的次数；无 routine 时统计该方向
任务转为 done 的次数。`minutes` 统计该方向所有已完成块的分钟数。
`expected` 为 `metric_target × 已过日历日数 / period 总日数`，包含查询日，
在 period 开始前为 0，结束后为完整目标；未设置目标则为 null。

`metric=none` 固定返回 `status=on_track`、`actual=null`、`expected=null`、
`weeks=[]`、`miss_streak=0`，不产生风险，也不接收上层风险状态。

`weeks` 按时间升序列出 period 内已结束的 ISO 周片段，包含周日查询的本周。
尚未结束的周不算未达标。月末提前结束的周片段在月末当天即可结算。
跨月周拆成两个日历片段：例如 2026-03-30、31 算三月，4 月 1 日起算四月。
同一个 ISO 周号可出现在相邻两个月的结果里。周实际只统计对应片段内的完成。

关联 routine 时周目标直接使用其 `target_count` 或 `target_minutes`，不按天数折算。
无 routine 时，从同方向、同 metric 的月计划取 `metric_target`，计算
`ceil(月目标 × 周片段在月内天数 / 月天数)`；上层跨月周累加各月片段的目标。
缺少匹配的月计划或目标时，不生成该周判定，不猜测周目标。

`miss_streak` 是序列尾部连续 `met=false` 的数量；周达标后归零。
月计划不足阈值时标 `behind`，不影响上层；达到阈值时标 `at_risk`，
沿 `parent_id` 影响量化的季、半年和年计划。上层自身序列达到阈值也标 `at_risk`。

`GET /settings/progress` 返回 `{ "streak_weeks": 3 }`（缺行默认 3）。
`PUT /settings/progress` 接受同形对象，需要人类 actor key，阈值为正整数。
设置以 JSON 数字保存在现有 `sin90_settings['progress.streak_weeks']`，
与 `setting.changed` 事件在同一事务写入。

## 状态与传导的补充口径

- **已放弃（`dropped`）的计划**仍然返回 actual / expected / weeks / miss_streak，方便回看，但 `status` 恒为 `on_track`：它不产生风险，也不把风险传给上层。已放弃的上层计划同样不会被标成 `at_risk`。放弃一个落后的月目标，是用户的明确决定，不应让还在进行的季、年计划一直显示风险。
- **`metric=none` 的中间层**本身不标状态，但风险会**穿过**它，继续传给更上层的量化计划。例如「量化月 → none 季 → 量化年」，月计划到了 `at_risk` 时，年计划也会变成 `at_risk`。
- **`at` 之后的完成**不计入。
