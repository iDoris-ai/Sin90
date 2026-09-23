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
//! 翻 PR 历史：
//!
//! - **真实 `ModelPort` 适配器（J10，等 `feat/t3.2.1-kernel-clients` 合并
//!   后）**：`src/adapter_agent24/clients/model.rs` 里 `ModelClient::new` +
//!   `impl ModelPort for ModelClient`（`call_with_timeout(125s)`，不带
//!   `request_id`）；`clients/error.rs` 的 `ClientError` 加
//!   `Unavailable{retryable, cause}` 与 `Cancelled` 两个变体，
//!   `map_rpc_error` 认 `unavailable`/`cancelled`，
//!   `every_spec_error_kind_maps_to_its_documented_variant` 的计数
//!   17 → 18。本分支没有类型化客户端可改，`ai::ports::ModelPort` 今天只有
//!   测试用的假实现。
//! - **manifest / 编译期隐私常量（J10c）**：`domain-os.yml` 加
//!   `kernel_capabilities: models`（`requires_models` 保持 `[]`）；新增
//!   cargo feature `remote-allowed-manifest` 与仅测试包用的
//!   `domain-os.remote-allowed.yml`；`include_str!` 驱动的
//!   `const MODEL_ACCESS`；子命令 `sin90 print-model-access`。正式包
//!   **不**声明 `remote_allowed`（§11.3.2 的硬约束）——这条钉子测试也留在
//!   这里补。
//! - **`allowed_ops`（`src/store/ai_port.rs`）随 T5.2.1/T5.3.1 收紧**：
//!   `Sin90Op::AssignTaskDirection`/`DraftReviewBody` 这两个 Op 在本分支
//!   还不存在，`allowed_ops(Classify)`/`allowed_ops(Summarize)` 因此暂时
//!   回答"空集"（提交必被拒），只有 `Propose`（复用既有
//!   `CarryOverTask`/`ReorderTasks`/`CreateTasks`）是真的。两个新 Op 落地
//!   后必须把这两个空集换成真实的单元素集合。
//! - **`AiReadModel::title_history` 是占位近似算法**（`store/ai_port.rs`
//!   的 `coarse_normalize`：空白折叠 + 小写）：§11.4.1 R1 的正式
//!   `normalize_title`（CJK 按字二元组等规则）是 T5.2.1
//!   `src/ai/classify.rs` 的交付物，落地后这里要换成调用它，而不是继续
//!   自己维护一份近似版本。
//! - **三个能力本身与它们的 HTTP 触发路由完全没做**：`POST
//!   /ai/{classify,summarize,propose}`、`GET /ai/runs/{run_id}`、进程内
//!   run 注册表（内存 LRU 64）、每能力单飞、模型调用信号量 2——这些是
//!   T5.2.1–T5.4.1 的范围，`ai::ladder::run_item` 已经是它们要调用的引擎
//!   梯核心，但组装它们（读候选、拼 prompt、解析输出、拼 `ProposalDraft`、
//!   起后台 run）都还没有对应的 `src/ai/{classify,summarize,propose}.rs`。
//!
//! What this module does NOT contain (out of T5.1.1's scope): the three
//! capabilities themselves (classify/summarize/propose, T5.2.1–T5.4.1), any
//! HTTP trigger route for them, and the real `_a24/model/complete` adapter
//! (`ModelPort` here is implemented only by test fakes — the
//! kernel-callback client lives on a separate branch,
//! `feat/t3.2.1-kernel-clients`, not yet merged here). See "T5.1.2 接线"
//! above for the full, itemized handoff list.

pub mod ladder;
pub mod ports;

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
    ModelRequest, ProposalDraft, ReadError, Role, ServedTier, SettingsRead, SinkError,
    UnavailableCause,
};
