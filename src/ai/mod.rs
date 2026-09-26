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
//! - ~~真实 `ModelPort` 适配器（J10，等 `feat/t3.2.1-kernel-clients` 合并
//!   后）~~ **T5.1.2 已关闭**：`src/adapter_agent24/clients/model.rs` 落地了
//!   `ModelClient::new` + `impl ModelPort for ModelClient`
//!   （`call_with_timeout(125s)`，不带 `request_id`）；`clients/error.rs` 的
//!   `ClientError` 加了 `Unavailable{retryable, cause}` 与 `Cancelled` 两个
//!   变体，`map_rpc_error` 认 `unavailable`/`cancelled`，
//!   `every_spec_error_kind_maps_to_its_documented_variant` 的计数
//!   17 → 18（J10）。`domain-os.yml` 加了 `kernel_capabilities: models`
//!   （不写 `model_access`，§2 #26 硬约束）；`remote-allowed-manifest` cargo
//!   feature + `domain-os.remote-allowed.yml`（只给测试包 B）；
//!   `ai::ports::MODEL_ACCESS` 编译期常量 + `sin90 print-model-access` 子命令
//!   （J10c、J23b）。`NoModelPort`（T5.2.1b 新增的占位类型）仍在——
//!   `standalone` 模式下三个 AI 触发路由依旧把它当 `model` 参数的具体类型传
//!   `None`，只有挂载模式下才换成真实 `ModelClient`（见下）。
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
//!   T5.2.1（顶层）关闭了 `POST /ai/classify` + `GET /ai/runs/{run_id}` 这两
//!   条 HTTP 路由、进程内 run 注册表（内存 LRU 64 近似、`BusyGuard` 保证
//!   panic 时也释放槽位）、每能力单飞（`src/http/ai_classify.rs` +
//!   `src/http/ai_runs.rs`）；T5.3.1/T5.4.1 补上了 `summarize`/`propose`
//!   （`src/ai/{summarize,propose}.rs` + 对应的 `src/http/ai_{summarize,
//!   propose}.rs`）。~~`src/http/ai_{classify,summarize,propose}.rs` 里
//!   `model: Option<&NoModelPort>` 与 `ModelAccess::LocalOnly` 目前都是硬编
//!   码~~ **T5.1.2 已关闭**：三处都改成挂载模式下经 `Sin90State::model`
//!   （`Option<Arc<dyn http::ModelCaller>>`——`http` 自己的 `dyn`-safe seam，
//!   同 `EventSink` 的做法，从不直接命名 `adapter_agent24::clients::model::
//!   ModelClient`）包出一个 `http::HttpModelPort`（真正实现
//!   `ai::ModelPort`，泛型参数 `M` 绑定到它）；`standalone` 模式下
//!   `Sin90State::model` 恒为 `None`，三处仍用 `NoModelPort` 做具体类型。
//!   `ModelAccess::LocalOnly` 硬编码换成 `ai::MODEL_ACCESS`（编译期常量，两
//!   种模式都读同一个值——`standalone` 下 `model` 恒 `None`，`plan()` 永远
//!   不会因为它调度 `Step::Model`，读哪个 `ModelAccess` 值不改变行为）。
//!   ~~进程内模型调用信号量 2（§11.4 公共）仍未实现~~ **2026-09-26 review
//!   （M3）已关闭**：三个能力（classify/summarize/propose）各自独立触发、各
//!   自可能同时有一次模型调用在途，内核对单模块的在途上限是 2——`http::
//!   SemaphoredModelCaller` 在 `wire_kernel_clients` 里包一层
//!   `tokio::sync::Semaphore(MODEL_MAX_IN_FLIGHT_PER_MODULE)`，建一次、经同
//!   一个 `Arc<dyn http::ModelCaller>` 被三个能力共享，第三路在进程内排队而
//!   不是打到内核换回一个 `busy`。
//!
//! What this module does NOT contain: nothing this doc used to list here is
//! still missing — `summarize`/`propose`, the real `_a24/model/complete`
//! adapter, the HTTP wiring, and the process-wide model-call semaphore have
//! all landed — see the itemized "T5.1.2 接线" list above for exactly which
//! task closed which bullet.
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
pub mod summarize;

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
    SummarizeBucket, SummarizeDraft, SummarizeRoutineRow, UnavailableCause, MANIFEST_YAML,
    MODEL_ACCESS,
};
