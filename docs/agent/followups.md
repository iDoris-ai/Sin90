# Sin90 跟进账本（不阻塞主线、但绝不能丢的事）

> 格式：`- [ ] SFU-N（级别）标题 —— 来源 · 去向判据`。主线做完后批量处理。

- [ ] SFU-1（C）中文法务文档：`LICENSE-zh.md` / `TRADEMARK-zh.md` / `CONTRIBUTING.md` / CLA workflow（可参照 `MushroomDAO/Sin90` 模板）—— 2026-09-20 待办 · 文件存在即关闭
- [ ] SFU-2（C）`MushroomDAO/Sin90` 空壳仓库与本仓库的命名关系 —— 需用户表态 · 用户给出结论即关闭
- [ ] SFU-3（B）`/packs/install` 非幂等（重复执行建第二套）—— Explore 2026-09-23（`store/packs.rs:47-53`）· M6 重做时关闭
- [ ] SFU-4（C）adapter 注释「min == max == 1」与 `PROTOCOL_MAX = 1000` 不符 —— `adapter_agent24/mod.rs:28-32` · MS 迁 SDK 时一并消失
- [ ] SFU-5（B）提议没有 `rejected` 的入口（无路由、无 store 函数）—— Explore 2026-09-23 · 加 reject 路由（require_human）后关闭

## T0.4 Codex 补审

（T0.4 执行时逐 commit 记录：结论 / 修复 PR / 转 followup）
- [ ] SFU-6（C）Rhythm 的 allocation 能否指向 `achieved`/`abandoned` 的 Direction —— T3.4.1 评审 L2 · 需要用户定是否只允许非终态 Direction
