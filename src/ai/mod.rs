//! T5.1.1 — the AI module's skeleton: engine ladder, call records, the
//! structural gate that keeps this module from ever writing `sin90.db`
//! directly (design DESIGN-LIFEOS.md §11.3/§11.5).
//!
//! `ai/` depends on NOTHING but `crate::core` and itself (§11.5 "依赖方
//! 向") — no `crate::store`, no `crate::http`, no `crate::adapter_agent24`.
//! That boundary is enforced three ways (§11.5's "三层判据"), only the first
//! of which lives in THIS crate:
//!   1. `tests/ai_boundary.rs` — a `syn`-based whitelist walk over every file
//!      under `src/ai/**/*.rs` (J7). `syn`/`proc-macro2` are dev-dependencies
//!      ONLY — `ai/`'s own `EXTERN_OK` list (see `tests/ai_boundary.rs`)
//!      does not include them, so the checker cannot live inside `ai/`
//!      itself without failing its own check.
//!   2. The only way `ai/` can ever write is through [`ports::AiSink`] —
//!      implemented by `Sin90Store` in `src/store/ai_port.rs`, which `ai/`
//!      never names.
//!   3. A behaviour-level table snapshot (J8, `tests/ai_boundary.rs`):
//!      running every `AiReadModel`/`AiSink` call this module can make must
//!      leave every table byte-for-byte unchanged except the two the sink
//!      itself is allowed to touch.
//!
//! Two files, both depth 2 for the boundary checker (`ai/mod.rs` is depth 1):
//!   - [`ports`]: the vocabulary and the three traits (`ModelPort`, `AiSink`,
//!     `AiReadModel`) — static shape, no control flow.
//!   - [`ladder`]: `plan()` and `run_item()` — the actual engine-ladder
//!     algorithm (degrade / defer / abort, the run-local circuit breaker and
//!     budget, `source_for()`, the privacy tripwire).
//!
//! # T5.1.2 接线 — 交给后续任务的清单
//!
//! T5.1.1 只落地引擎梯骨架本身；下面每一条都是本分支刻意不做、且做不了
//! （缺依赖）或不该在本分支做（超出范围）的事，统筹复核（2026-09-24）已
//! 确认接受，记在这里而不是只记在 PR 描述里，免得下一个任务的实现者要去
//! 翻 PR 历史。T5.2.1（classify，本文件同批次）已经关掉了其中两条——保留
//! 记录，标注关闭状态：
//!
//! - **真实 `ModelPort` 适配器（J10，等 `feat/t3.2.1-kernel-clients` 合并
//!   后）**：`src/adapter_agent24/clients/model.rs` 里 `ModelClient::new` +
//!   `impl ModelPort for ModelClient`（`call_with_timeout(125s)`，不带
//!   `request_id`）；`clients/error.rs` 的 `ClientError` 加
//!   `Unavailable{retryable, cause}` 与 `Cancelled` 两个变体，
//!   `map_rpc_error` 认 `unavailable`/`cancelled`，
//!   `every_spec_error_kind_maps_to_its_documented_variant` 的计数
//!   17 → 18。**仍未做**：`ai::ports::ModelPort` 今天只有测试用的假实现和
//!   `NoModelPort`（T5.2.1b 新增的占位类型——没有真实客户端时，调用方仍能
//!   给引擎梯的泛型参数一个具体类型；`POST /ai/classify` 会是它第一个真正
//!   的调用方，但那条路由本身不在本分支）；`domain-os.yml`/manifest 相关
//!   的 J10c 同样未做。
//! - ~~`allowed_ops`（`src/store/ai_port.rs`）随 T5.2.1/T5.3.1 收紧~~
//!   **T5.2.1 已关闭其中 `Classify` 一半**：`Sin90Op::AssignTaskDirection`
//!   落地，`allowed_ops(Classify)` 现在回答真实的单元素集合
//!   `{AssignTaskDirection}`。`allowed_ops(Summarize)` 仍是空集——
//!   `Sin90Op::DraftReviewBody`（T5.3.1）不在本任务范围内（明确不做
//!   summarize），留给下一个任务收紧。
//! - ~~`AiReadModel::title_history` 是占位近似算法~~ **T5.2.1 已关闭**：
//!   `store/ai_port.rs` 的 `title_history` 现在调用本模块
//!   [`classify::normalize_title`]（§11.4.1 R1 的正式算法），并在 SQL 里
//!   过滤掉归属已终结（achieved/abandoned）Direction 的历史任务；不再维护
//!   一份自己的近似版本。
//! - **classify 能力本身**：T5.2.1b 交付了 [`classify`] 模块（
//!   `normalize_title`、R1/R2、模型 request/schema/parse、
//!   `classify_one`/`run_classify` 驱动、`select_targets` 输入校验）。
//!   ~~仍未做，留给上一层的 T5.2.1（http）分支~~ **T5.2.1（顶层）已关闭**：
//!   `POST /ai/classify` + `GET /ai/runs/{run_id}` 这两条 HTTP 路由、进程内
//!   run 注册表（内存 LRU 64 近似、`BusyGuard` 保证 panic 时也释放槽位）、
//!   每能力单飞都已落地（`src/http/ai_classify.rs` + `src/http/ai_runs.rs`）。
//!   `src/http/ai_classify.rs` 里 `model: Option<&NoModelPort>` 与
//!   `ModelAccess::LocalOnly` 目前都是硬编码——没有真实 `ModelPort` 适配器
//!   可传、也没有 `domain-os.yml` 的 `model_access` 可读，这两处都留了
//!   `TODO(T5.1.2)` 注释，等真实内核客户端接上后才能从硬编码变成真正的
//!   运行时值。`summarize`/`propose`（T5.3.1/T5.4.1）仍不在这里——
//!   `ai::ladder::run_item` 这个共享的引擎梯核心已经是它们也要调用的东西，
//!   但组装它们各自的候选读取/prompt/复核/`ProposalDraft` 还没有对应的
//!   `src/ai/{summarize,propose}.rs`；进程内模型调用信号量 2（§11.4 公共）
//!   也还没有实现——现在没有真实并发模型调用需要限流，`model` 参数在
//!   `POST /ai/classify` 的触发路径上恒为 `None`。
//!
//! What this module does NOT contain: `summarize`/`propose` (T5.3.1/T5.4.1)
//! and the real `_a24/model/complete` adapter (`ModelPort` here is
//! implemented only by test fakes and `NoModelPort` — the kernel-callback
//! client lives on a separate branch, `feat/t3.2.1-kernel-clients`, not yet
//! merged here). The HTTP trigger routes for `classify` themselves ARE
//! delivered (see above) — this bullet used to say otherwise when this doc
//! was written for the T5.2.1b layer alone; updated at the top of the stack
//! where that claim stopped being true. See "T5.1.2 接线" above for the
//! full, itemized handoff list.
//!
//! # 已知限制（2026-09-24 评审，接受，不在本轮改）
//!
//! - **`privacy_tripwire` 之后不熔断该引擎**：`tripwire()` 命中时只把这一
//!   步降级（弃用远端回复），run 剩余条目仍会再请求同一引擎——设计原文
//!   如此（§11.3.4 的熔断表只列 `no_provider`/`backend_config`/
//!   `forbidden` 三种，`privacy_tripwire` 不在表内）。列为待重新考虑：
//!   如果远端在开关关闭期间持续把 `simple` 调用送到远端（R3 的已知场景），
//!   本 run 会反复触发绊线而不是提早放弃。
//! - **`Defer` 之后当前条目仍先跑过 `ReflexDecisive`**：`run_item` 对
//!   `classify` 的梯是 `[ReflexDecisive, Model(..), ReflexFallback]`；
//!   `ReflexDecisive` 本身不消耗调用预算/不检查 `models_off`，所以即使
//!   上一条目已经把 `models_off` 置位，下一条目仍会先跑一次
//!   `ReflexDecisive`（可能产出 `undecided` 调用记录）才走到
//!   `Model(..)` 步被 `Deferred` 短路。纯粹是统计噪声（多几行
//!   `error_kind = 'undecided'` 的调用记录），不影响正确性
//!   （§11.3.5 L5：`undecided` 本就不算入降级统计）。

#![forbid(unsafe_code)]

pub mod classify;
pub mod ladder;
pub mod ports;
pub mod propose;

// `self::`-prefixed, not bare (`use ladder::…`): the boundary checker
// (`tests/ai_boundary.rs`, J7) deliberately does not trust a bare `use`
// path's first segment to be "obviously" a local sibling module — Rust's
// 2018 path resolution would let it ALSO mean an external crate of the same
// name, which is exactly the kind of ambiguity the checker refuses to
// special-case for `use` items (see `Checker::check`'s `in_use` doc).
pub use self::ladder::{
    plan, run_item, source_for, tripwire, Outcome, RunState, Step, MAX_MODEL_CALLS_PER_RUN,
    RUN_DEADLINE_SECS,
};
pub use self::ports::{
    AiCallRecord, AiReadModel, AiSettings, AiSink, Capability, Complexity, DirectionCandidate,
    Engine, LadderAction, ModelAccess, ModelFailure, ModelMessage, ModelPort, ModelReply,
    ModelRequest, NoModelPort, ProposalDraft, ReadError, Role, ServedTier, SettingsRead, SinkError,
    SummarizeBucket, SummarizeDraft, SummarizeRoutineRow, UnavailableCause,
};
