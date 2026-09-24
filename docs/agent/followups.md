# Sin90 跟进账本（不阻塞主线、但绝不能丢的事）

> 格式：`- [ ] SFU-N（级别）标题 —— 来源 · 去向判据`。主线做完后批量处理。

- [ ] SFU-1（C）中文法务文档：`LICENSE-zh.md` / `TRADEMARK-zh.md` / `CONTRIBUTING.md` / CLA workflow（可参照 `MushroomDAO/Sin90` 模板）—— 2026-09-20 待办 · 文件存在即关闭
- [ ] SFU-2（C）`MushroomDAO/Sin90` 空壳仓库与本仓库的命名关系 —— 需用户表态 · 用户给出结论即关闭
- [ ] SFU-3（B）`/packs/install` 非幂等（重复执行建第二套）—— Explore 2026-09-23（`store/packs.rs:47-53`）· M6 重做时关闭
- [ ] SFU-4（C）adapter 注释「min == max == 1」与 `PROTOCOL_MAX = 1000` 不符 —— `adapter_agent24/mod.rs:28-32` · MS 迁 SDK 时一并消失
- [ ] SFU-5（B）提议没有 `rejected` 的入口（无路由、无 store 函数）—— Explore 2026-09-23 · 加 reject 路由（require_human）后关闭

- [ ] SFU-7（B）SIGTERM 与 on_fatal 统一成一条优雅关闭路径 —— T3.2.0 第 2 轮评审 N-M5 · 现状：on_fatal 直接 `exit(70)`、SIGTERM 也无优雅处理；进行中的 HTTP 请求被重置（已提交但未响应的非幂等 POST 可能被客户端重试）、已提交事务对应的镜像事件丢失（SQLite 原子性不受影响）。做法：on_fatal 与 SIGTERM 都只触发一个 CancellationToken，`axum::serve(...).with_graceful_shutdown` 排空 ≤2s 再退出（< 内核 EXIT_SETTLE 100ms + STOP_GRACE 3s）。判据：排空期间在途请求完成；超时后仍退出

- [ ] SFU-8（B，需用户定）周复盘草稿的 `routines[].completed` 恒为 0 —— T4.3.1 · ScheduleBlock 没有 routine 关联，`routine.fired` 事件也不带 block/task 引用，事件回放无法把完成的 block 归到某个 Routine。需要决定关联方式（例如 fired 时自动建一个带 `routine_id` 的 ScheduleBlock，或 block 创建时可选 `routine_id`，均为数据模型改动，先进 DESIGN §2）。判据：完成一个由 Routine 产生的 block → 草稿 completed +1；正对照：普通 block 不计入

- [x] SFU-9（B）✅ #29 合并 2026-09-24 `Sin90Op` 枚举没有 `deny_unknown_fields`，带多余字段的 op 被静默接受 —— T5.0.1 设计 §11.1 F-1（scratch 已复现）· 判据：多一个未知字段的 op 提交 → 400；正对照：合法 op 接受
- [x] SFU-10（B）✅ #29 合并 2026-09-24 `submit_proposal` 提交时不跑 validate（只在 accept 时跑）—— T5.0.1 设计 §11.1 F-2 · 判据：提交结构非法的提议 → 422 且不落库；正对照：合法提议 202
- [ ] SFU-11（B）`ClientError` 不认内核的 `unavailable`（ME4 推理回调新增）与已有的 `cancelled`，都落到 Other —— T5.0.1 设计 · 判据：两个 kind 各映射到专门变体并有正确的重试分类；随 ME4 推理回调合并后处理
- [ ] SFU-12（C）J7 syn 白名单检查器把 `pub(crate)` 的可见性限定当成路径审而误拒（`crate path must go through core or ai`），迫使 `is_cf_format_char`/`gap_directions` 改为 `pub` —— T5.4.1 R2 · 判据：检查器跳过 `visit_vis_restricted`，`pub(crate) fn` 放行；负对照：`crate::store::…` 路径仍被拒
- [ ] SFU-13（B）`CreateTasks` 的 validate 不查每个标题非空、`direction_id` 存在且未终结；「只能是缺口 Direction」只在生成时成立，accept 时 Direction 可能已 abandoned —— T5.4.1 R1 L5 · 判据：accept 指向 abandoned Direction 的 CreateTasks → 422；正对照：指向 active Direction 成功
- [ ] SFU-14（C）AI precheck 拿不到写锁时返回全 false，去重 fail-open，可能产出重复挂起提议（classify/propose 同）—— T5.4.1 R2 L-6 · 判据：锁竞争下重复触发不产重复提议，或在 run 结果里标注 dedup_degraded；正对照：无竞争时去重生效

## T0.4 Codex 补审

（T0.4 执行时逐 commit 记录：结论 / 修复 PR / 转 followup）
- [ ] SFU-6（C）Rhythm 的 allocation 能否指向 `achieved`/`abandoned` 的 Direction —— T3.4.1 评审 L2 · 需要用户定是否只允许非终态 Direction
