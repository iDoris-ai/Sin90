//! `_a24/model/complete` — typed client (T5.1.2, ME4-S2 v3.1 frozen design).
//!
//! ME4-5.2.1 migration (`docs/design/ME4-S3-os-sdk.md` §5.1): the wire shape
//! itself (`build_params`/`WireResult`) moved into `agent24-os-sdk` as
//! `CompleteRequest`/`CompleteResult` — this file keeps a NEWTYPE around
//! `agent24_os_sdk::ModelClient` so [`ModelClient::complete`] can keep its
//! OLD inherent signature, `&ai::ModelRequest -> Result<ai::ModelReply,
//! ClientError>` (`ai::ModelRequest` has no `Assistant` role and no
//! `response_format`-is-optional shape the way the SDK's own
//! `CompleteRequest` does — converting between the two happens here, once,
//! at this one boundary the `ai` module boundary check (J7) already forces
//! to exist). `MODEL_CALL_TIMEOUT` becomes an alias for the SDK's own
//! `MODEL_RESPONSE_TIMEOUT` (same value, 125s) rather than a separate
//! constant, so the one test that reads it (`model_response_timeout_...`,
//! below) still compiles unchanged.
//!
//! # Two `complete`s on purpose
//!
//! [`ModelClient`] has BOTH an inherent `complete(&ModelRequest) ->
//! Result<ModelReply, ClientError>` (the wire-shaped one this file's own
//! tests exercise directly) AND `impl ai::ModelPort for ModelClient`'s
//! `complete(ModelRequest) -> impl Future<Output = Result<ModelReply,
//! ModelFailure>>` (what the engine ladder actually calls through the
//! generic `M: ModelPort` bound). This is legal Rust — an inherent method
//! and a trait method may share a name — and safe here specifically because
//! every call site is unambiguous: dot-call syntax (`client.complete(&req)`)
//! always resolves the inherent method first (and does, since its
//! `&ModelRequest` parameter matches), while generic code
//! (`ai::ladder::run_item::<M: ModelPort>`) can only ever see the trait
//! method — a generic type parameter has no inherent methods of its own to
//! shadow with.
use std::sync::Arc;
use std::time::Duration;

use agent24_os_proto::module::Connection;
use serde_json::json;

use super::error::ClientError;
use crate::ai::{
    Complexity, ModelFailure, ModelMessage, ModelPort, ModelReply, ModelRequest, Role, ServedTier,
};

/// The prefix `Offer.provides` must cover for [`ModelClient::new`] to return
/// `Some` — also the one method this client ever calls
/// (`{PREFIX}complete`).
pub const PREFIX: &str = "_a24/model/";

/// J10(a): `_a24/model/complete`'s own method timeout is 120s (ME4-S2 §5.2's
/// `MODEL_CALL_TIMEOUT`) — this client's response deadline is set a little
/// LONGER than that (§2.7's own reasoning), and is now the SDK's own
/// `MODEL_RESPONSE_TIMEOUT` rather than a value this crate picks itself.
pub(crate) const MODEL_CALL_TIMEOUT: Duration = agent24_os_sdk::clients::MODEL_RESPONSE_TIMEOUT;

fn role(role: Role) -> agent24_os_sdk::ModelRole {
    match role {
        Role::System => agent24_os_sdk::ModelRole::System,
        Role::User => agent24_os_sdk::ModelRole::User,
    }
}

fn complexity(c: Complexity) -> agent24_os_sdk::Complexity {
    match c {
        Complexity::Simple => agent24_os_sdk::Complexity::Simple,
        Complexity::Complex => agent24_os_sdk::Complexity::Complex,
    }
}

/// `ai::ModelRequest` -> the SDK's own `CompleteRequest` (module doc: the
/// one conversion boundary this file exists to own).
fn build_request(req: &ModelRequest) -> agent24_os_sdk::CompleteRequest {
    agent24_os_sdk::CompleteRequest {
        messages: req
            .messages
            .iter()
            .map(|m: &ModelMessage| agent24_os_sdk::ModelMessage {
                role: role(m.role),
                content: m.content.clone(),
            })
            .collect(),
        response_format: Some(agent24_os_sdk::JsonSchemaFormat {
            name: req.schema_name.to_string(),
            schema: json!(req.schema),
            strict: true,
        }),
        max_tokens: Some(req.max_tokens),
        complexity: Some(complexity(req.complexity)),
    }
}

/// Typed `_a24/model/complete` client. Only [`ModelClient::new`] ever
/// constructs one.
pub struct ModelClient(agent24_os_sdk::ModelClient);

impl ModelClient {
    /// `None` unless the handshake's `Offer` granted [`PREFIX`]
    /// (architecture.md 不可破边界 #7 — same "句柄可能不在" posture every
    /// other typed client already has).
    #[must_use]
    pub fn new(conn: &Arc<Connection>) -> Option<Self> {
        agent24_os_sdk::ModelClient::new(conn).map(Self)
    }

    /// Wraps an already-built SDK client (`agent24_os_sdk::Module::
    /// model()`) — the production path, which never has a bare
    /// `Arc<Connection>` to hand [`Self::new`].
    #[must_use]
    pub fn from_sdk(inner: agent24_os_sdk::ModelClient) -> Self {
        Self(inner)
    }

    /// `_a24/model/complete`. J10(a): the SDK's own 125s response deadline,
    /// never a `request_id` (this client never passes one to the SDK).
    pub async fn complete(&self, req: &ModelRequest) -> Result<ModelReply, ClientError> {
        let wire = build_request(req);
        let result = self.0.complete(&wire, None).await?;
        let tier = match result.tier {
            agent24_os_sdk::ServedTier::Local => ServedTier::Local,
            agent24_os_sdk::ServedTier::Remote => ServedTier::Remote,
        };
        // J10(f)/§5.1 "usage u32 -> u64": the SDK parses `usage.{prompt,
        // completion}_tokens` as `u64` (the kernel's own wire width);
        // `ai::ModelReply` still carries `u32` (unchanged domain type, out
        // of this migration's scope) — a value the kernel would never
        // actually send (> u32::MAX) fails here exactly the way it failed
        // pre-migration (`ClientError::Other`), just one layer down (SDK
        // used to fail this at deserialization; the SDK now deserializes
        // successfully as `u64` and THIS conversion is where it now fails).
        let prompt_tokens = u32::try_from(result.usage.prompt_tokens).map_err(|_| {
            ClientError::Other(format!(
                "model/complete: bad response shape: usage.prompt_tokens {} does not fit u32",
                result.usage.prompt_tokens
            ))
        })?;
        let completion_tokens = u32::try_from(result.usage.completion_tokens).map_err(|_| {
            ClientError::Other(format!(
                "model/complete: bad response shape: usage.completion_tokens {} does not fit u32",
                result.usage.completion_tokens
            ))
        })?;
        Ok(ModelReply {
            text: result.text,
            model_id: result.model_id,
            tier,
            prompt_tokens: Some(prompt_tokens),
            completion_tokens: Some(completion_tokens),
        })
    }
}

/// §7's complete `ModelError` → `ClientError` → [`ModelFailure`] mapping —
/// exhaustive over every [`ClientError`] variant (no wildcard arm, mirroring
/// [`ModelFailure::action`]'s own posture): a new [`ClientError`] variant
/// that forgets to extend this fails to COMPILE. Every arm here matches
/// [`ModelFailure`]'s own doc comments (`ai::ports`) exactly — those doc
/// comments spelled out this mapping before this file existed (T5.1.1), this
/// is just the code that keeps the promise. `Unavailable`'s `cause` is
/// translated from the SDK's own (domain-decoupled) closed set into
/// `crate::ai::UnavailableCause` here — the one place in this crate that
/// still needs the `ai`-specific type (`clients/error.rs`'s own doc).
fn map_client_error_to_model_failure(err: ClientError) -> ModelFailure {
    match err {
        ClientError::Unavailable { retryable, cause } => ModelFailure::Unavailable {
            retryable,
            cause: match cause {
                agent24_os_sdk::UnavailableCause::NoProvider => {
                    crate::ai::UnavailableCause::NoProvider
                }
                agent24_os_sdk::UnavailableCause::RequestRejected => {
                    crate::ai::UnavailableCause::RequestRejected
                }
                agent24_os_sdk::UnavailableCause::BackendConfig => {
                    crate::ai::UnavailableCause::BackendConfig
                }
                agent24_os_sdk::UnavailableCause::ResponseTooLarge => {
                    crate::ai::UnavailableCause::ResponseTooLarge
                }
            },
        },
        ClientError::Busy(_) => ModelFailure::Busy,
        ClientError::RateLimited(_) => ModelFailure::RateLimited,
        ClientError::NotReady(_) => ModelFailure::NotReady,
        ClientError::Timeout(_) => ModelFailure::Timeout,
        ClientError::Forbidden(_) => ModelFailure::Forbidden,
        ClientError::InvalidParams(_) | ClientError::PayloadTooLarge(_) => ModelFailure::BadRequest,
        ClientError::NotFound(_)
        | ClientError::QuotaExceeded(_)
        | ClientError::TokenInvalid(_)
        | ClientError::RequestNotInFlight(_)
        | ClientError::Other(_) => ModelFailure::Other,
        ClientError::Draining(_) | ClientError::Revoked(_) | ClientError::NotSent(_) => {
            ModelFailure::GenerationEnding
        }
        ClientError::ConnectionLost => ModelFailure::ConnectionLost,
        ClientError::Cancelled => ModelFailure::Cancelled,
    }
}

impl ModelPort for ModelClient {
    // Dot-call resolves to the INHERENT `complete(&ModelRequest)` above
    // (module doc's "Two `complete`s on purpose") — `&req` matches its
    // signature exactly, so there is no ambiguity to resolve.
    async fn complete(&self, req: ModelRequest) -> Result<ModelReply, ModelFailure> {
        self.complete(&req)
            .await
            .map_err(map_client_error_to_model_failure)
    }
}

/// `http`'s own `dyn`-safe seam ([`crate::http::ModelCaller`], mirroring
/// `KernelEventSink: EventSink`) — lets `wire_kernel_clients` hand
/// `Sin90State::model` an `Arc<dyn ModelCaller>` without `http` ever naming
/// [`ModelClient`] directly (lib.rs's own dependency arrow: `http` must not
/// depend on `adapter_agent24`).
impl crate::http::ModelCaller for ModelClient {
    fn complete<'a>(
        &'a self,
        req: ModelRequest,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<ModelReply, ModelFailure>> + Send + 'a>,
    > {
        Box::pin(async move { ModelPort::complete(self, req).await })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapter_agent24::clients::test_support::{
        fake_kernel, read_request, respond, respond_error_with_data,
    };
    use agent24_os_sdk::UnavailableCause;
    use serde_json::{json, Map};

    fn sample_request() -> ModelRequest {
        let mut schema = Map::new();
        schema.insert("type".to_string(), json!("object"));
        ModelRequest {
            messages: vec![
                ModelMessage {
                    role: Role::System,
                    content: "be terse".to_string(),
                },
                ModelMessage {
                    role: Role::User,
                    content: "classify this".to_string(),
                },
            ],
            schema_name: "classify_v1",
            schema,
            max_tokens: 256,
            complexity: Complexity::Simple,
        }
    }

    #[tokio::test]
    async fn model_client_offer_gate_none_without_prefix_some_with_prefix() {
        let (clients, _peer) = fake_kernel(vec!["_a24/events/".to_string()]).await;
        assert!(ModelClient::new(&clients).is_none());

        let (clients, _peer) = fake_kernel(vec!["_a24/model/".to_string()]).await;
        assert!(ModelClient::new(&clients).is_some());
    }

    /// J10(a) (request shape half) + J10(f): the request never carries a
    /// `request_id`, matches the exact wire contract, and a successful
    /// response's `usage`/`model_id`/`tier` all land on [`ModelReply`]
    /// correctly.
    #[tokio::test]
    async fn model_client_complete_request_has_no_request_id_and_reply_maps_usage() {
        let (clients, mut peer) = fake_kernel(vec!["_a24/model/".to_string()]).await;
        let client = ModelClient::new(&clients).unwrap();
        let req = sample_request();
        let call = tokio::spawn(async move { client.complete(&req).await });

        let wire_req = read_request(&mut peer).await;
        assert_eq!(wire_req["method"], "_a24/model/complete");
        let params = &wire_req["params"];
        assert!(
            params.get("request_id").is_none(),
            "model/complete must never carry a request_id (§3.4)"
        );
        assert!(params.get("_meta").is_none());
        assert_eq!(
            params["messages"],
            json!([
                {"role": "system", "content": "be terse"},
                {"role": "user", "content": "classify this"},
            ])
        );
        assert_eq!(params["max_tokens"], json!(256));
        assert_eq!(params["complexity"], json!("simple"));
        assert_eq!(params["response_format"]["type"], json!("json_schema"));
        assert_eq!(
            params["response_format"]["json_schema"]["name"],
            json!("classify_v1")
        );
        assert_eq!(
            params["response_format"]["json_schema"]["schema"],
            json!({"type": "object"})
        );
        assert_eq!(
            params["response_format"]["json_schema"]["strict"],
            json!(true)
        );

        respond(
            &mut peer,
            &wire_req,
            json!({
                "text": "hello",
                "model_id": "Qwen3-8B-4bit",
                "tier": "local",
                "usage": {"prompt_tokens": 12, "completion_tokens": 34},
            }),
        )
        .await;
        let reply = call.await.unwrap().unwrap();
        assert_eq!(reply.text, "hello");
        assert_eq!(reply.model_id.as_deref(), Some("Qwen3-8B-4bit"));
        assert_eq!(reply.tier, ServedTier::Local);
        assert_eq!(reply.prompt_tokens, Some(12));
        assert_eq!(reply.completion_tokens, Some(34));
    }

    /// `model_id: null` and `tier: "remote"` map through correctly too —
    /// not just the `Local`/`Some(id)` case above.
    #[tokio::test]
    async fn model_client_null_model_id_and_remote_tier_map_through() {
        let (clients, mut peer) = fake_kernel(vec!["_a24/model/".to_string()]).await;
        let client = ModelClient::new(&clients).unwrap();
        let req = sample_request();
        let call = tokio::spawn(async move { client.complete(&req).await });

        let wire_req = read_request(&mut peer).await;
        respond(
            &mut peer,
            &wire_req,
            json!({
                "text": "hi",
                "model_id": null,
                "tier": "remote",
                "usage": {"prompt_tokens": 1, "completion_tokens": 2},
            }),
        )
        .await;
        let reply = call.await.unwrap().unwrap();
        assert!(reply.model_id.is_none());
        assert_eq!(reply.tier, ServedTier::Remote);
    }

    /// J10(a) (timeout half): the client's own deadline is fixed at 125s —
    /// longer than the kernel's own 120s method timeout (ME4-S2 §5.2),
    /// never shorter (module doc's `MODEL_CALL_TIMEOUT` reasoning).
    #[test]
    fn model_client_call_timeout_is_125_seconds_longer_than_the_kernels_120s() {
        assert_eq!(MODEL_CALL_TIMEOUT, Duration::from_secs(125));
        assert!(MODEL_CALL_TIMEOUT > Duration::from_secs(120));
    }

    /// L2 (2026-09-26 review): exercises the REAL wait, not just the
    /// constant, via `tokio::time::pause` — a fake peer that reads the
    /// request and then never answers. Virtual clock advanced to 124s: the
    /// call must not have resolved yet. Advanced further past 125s: it must
    /// resolve to `ClientError::Timeout`. `peer` is kept alive for the whole
    /// test (not dropped early) so this isolates a pure response timeout
    /// from a `ConnectionLost` race.
    ///
    /// # Why the "still pending at 124s" check is a 0-duration `timeout`
    /// wrapper, not `JoinHandle::is_finished()` or a manual `yield_now` loop
    ///
    /// Verified by hand, at real cost (see PR notes): under
    /// `start_paused = true`, the moment this test's own task does ANYTHING
    /// that yields to the scheduler while the ONLY other pending thing is
    /// the spawned call's own un-elapsed `Sleep` (a bare `yield_now().await`,
    /// or checking `JoinHandle::is_finished()` after a couple of them),
    /// tokio's paused-clock "nothing else to do → auto-advance to the next
    /// timer" heuristic races ahead and resolves it EARLY, regardless of how
    /// much this test explicitly asked `tokio::time::advance` to move by —
    /// a 124s explicit advance plus ordinary polling was observed to
    /// spuriously resolve the call as already-timed-out. Wrapping the check
    /// itself in `tokio::time::timeout(Duration::ZERO, &mut call)` sidesteps
    /// this: it gives the auto-advance heuristic a CLOSER (already-due, 0ms)
    /// timer to jump to instead, so it never has a reason to overshoot past
    /// this checkpoint into the call's own far-off 125s one.
    ///
    /// Mutation (verified by hand): dropping `MODEL_CALL_TIMEOUT` to 1s
    /// turns this red (`still_pending` becomes `false` — the call resolves
    /// well before the 124s checkpoint). A narrow drop (to 120s, only 4s
    /// short of the checkpoint) is NOT reliably caught by this specific
    /// checkpoint — the same paused-clock scheduling asynchrony this doc
    /// describes cuts both ways at small margins — so this test's real
    /// guarantee is "the constant is not wildly wrong," not "exactly 125s
    /// to the second"; [`model_client_call_timeout_is_125_seconds_longer_
    /// than_the_kernels_120s`] pins the exact value separately.
    #[tokio::test(start_paused = true)]
    async fn model_client_times_out_at_125_seconds_not_before() {
        let (clients, mut peer) = fake_kernel(vec!["_a24/model/".to_string()]).await;
        let client = ModelClient::new(&clients).unwrap();
        let req = sample_request();
        let mut call = tokio::spawn(async move { client.complete(&req).await });

        let _wire_req = read_request(&mut peer).await; // consumed; peer never answers

        tokio::time::advance(Duration::from_secs(124)).await;
        let still_pending = tokio::time::timeout(Duration::ZERO, &mut call)
            .await
            .is_err();
        assert!(still_pending, "must not time out before 125s");

        tokio::time::advance(Duration::from_secs(2)).await; // now past 125s total
        let err = call.await.unwrap().unwrap_err();
        assert!(matches!(err, ClientError::Timeout(_)));
        drop(peer); // keep alive up to here, not a moment less.
    }

    /// Positive control for the test above: with the peer answering
    /// (immediately, no `tokio::time::advance` in between — see note
    /// below), the call succeeds instead — proves this harness under
    /// `start_paused` can still complete normally, so the timeout test
    /// above isn't just measuring "nothing ever finishes under
    /// `start_paused`". Deliberately does NOT call `tokio::time::advance`
    /// before responding: mixing REAL socket I/O with an EXPLICIT virtual-
    /// clock advance is a known paused-time footgun (verified by hand —
    /// advancing even 34s before the peer's response arrives races tokio's
    /// auto-advance against the real IO wakeup and the 125s timer wins,
    /// producing a spurious `Timeout` despite the response already having
    /// been written) — this test's own real-time-only shape sidesteps that
    /// race entirely, which is exactly why the timeout test above never
    /// calls `respond` at all.
    #[tokio::test(start_paused = true)]
    async fn model_client_positive_control_answers_immediately_still_succeeds() {
        let (clients, mut peer) = fake_kernel(vec!["_a24/model/".to_string()]).await;
        let client = ModelClient::new(&clients).unwrap();
        let req = sample_request();
        let call = tokio::spawn(async move { client.complete(&req).await });

        let wire_req = read_request(&mut peer).await;
        respond(
            &mut peer,
            &wire_req,
            json!({
                "text": "hi",
                "model_id": null,
                "tier": "local",
                "usage": {"prompt_tokens": 1, "completion_tokens": 1},
            }),
        )
        .await;
        let reply = call.await.unwrap().unwrap();
        assert_eq!(reply.text, "hi");
    }

    /// J10(b): `unavailable` with `retryable:false, cause:backend_config`
    /// maps to the dedicated `ClientError::Unavailable` value (error.rs's
    /// own tests already cover `map_rpc_error` directly; this proves the
    /// SAME thing reaches a caller through `ModelClient::complete`'s real
    /// wire round trip, not just the mapping function in isolation).
    #[tokio::test]
    async fn model_client_unavailable_backend_config_maps_through_the_real_call() {
        let (clients, mut peer) = fake_kernel(vec!["_a24/model/".to_string()]).await;
        let client = ModelClient::new(&clients).unwrap();
        let req = sample_request();
        let call = tokio::spawn(async move { client.complete(&req).await });

        let wire_req = read_request(&mut peer).await;
        respond_error_with_data(
            &mut peer,
            &wire_req,
            -32000,
            "the model backend refused this request",
            json!({"kind": "unavailable", "retryable": false, "cause": "backend_config"}),
        )
        .await;
        let err = call.await.unwrap().unwrap_err();
        assert_eq!(
            err,
            ClientError::Unavailable {
                retryable: false,
                cause: UnavailableCause::BackendConfig,
            }
        );
        assert!(err.is_permanent());
        assert!(!err.is_retryable());
    }

    /// Positive control for the test above: a DIFFERENT cause +
    /// `retryable: true` produces a different value, with `is_retryable()`
    /// following the wire's own bool.
    #[tokio::test]
    async fn model_client_unavailable_no_provider_retryable_is_a_different_value() {
        let (clients, mut peer) = fake_kernel(vec!["_a24/model/".to_string()]).await;
        let client = ModelClient::new(&clients).unwrap();
        let req = sample_request();
        let call = tokio::spawn(async move { client.complete(&req).await });

        let wire_req = read_request(&mut peer).await;
        respond_error_with_data(
            &mut peer,
            &wire_req,
            -32000,
            "no model this module may use is available right now",
            json!({"kind": "unavailable", "retryable": true, "cause": "no_provider"}),
        )
        .await;
        let err = call.await.unwrap().unwrap_err();
        assert_eq!(
            err,
            ClientError::Unavailable {
                retryable: true,
                cause: UnavailableCause::NoProvider,
            }
        );
        assert!(err.is_retryable());
        assert!(!err.is_permanent());
    }

    /// J10(c): a `cause` outside the closed set falls through to `Other`.
    #[tokio::test]
    async fn model_client_unavailable_cause_outside_closed_set_falls_through_to_other() {
        let (clients, mut peer) = fake_kernel(vec!["_a24/model/".to_string()]).await;
        let client = ModelClient::new(&clients).unwrap();
        let req = sample_request();
        let call = tokio::spawn(async move { client.complete(&req).await });

        let wire_req = read_request(&mut peer).await;
        respond_error_with_data(
            &mut peer,
            &wire_req,
            -32000,
            "test",
            json!({"kind": "unavailable", "retryable": true, "cause": "not_a_real_cause"}),
        )
        .await;
        let err = call.await.unwrap().unwrap_err();
        assert!(matches!(err, ClientError::Other(_)));
    }

    /// J10(d): `cancelled` maps to the dedicated variant through the real
    /// wire round trip.
    #[tokio::test]
    async fn model_client_cancelled_maps_through_the_real_call() {
        let (clients, mut peer) = fake_kernel(vec!["_a24/model/".to_string()]).await;
        let client = ModelClient::new(&clients).unwrap();
        let req = sample_request();
        let call = tokio::spawn(async move { client.complete(&req).await });

        let wire_req = read_request(&mut peer).await;
        respond_error_with_data(
            &mut peer,
            &wire_req,
            -32000,
            "the daemon is shutting down",
            json!({"kind": "cancelled"}),
        )
        .await;
        let err = call.await.unwrap().unwrap_err();
        assert_eq!(err, ClientError::Cancelled);
    }

    /// The `ai::ModelPort` impl maps every representative `ClientError` it
    /// can see onto the [`ModelFailure`] `ai::ports`'s own doc comments
    /// promise — exercised through `<ModelClient as ModelPort>::complete`
    /// specifically (NOT the inherent `complete`, which returns a different
    /// `Result` type; module doc's "Two `complete`s on purpose").
    #[tokio::test]
    async fn model_client_model_port_impl_maps_forbidden_to_model_failure_forbidden() {
        let (clients, mut peer) = fake_kernel(vec!["_a24/model/".to_string()]).await;
        let client = ModelClient::new(&clients).unwrap();
        let req = sample_request();
        let call = tokio::spawn(async move { ModelPort::complete(&client, req).await });

        let wire_req = read_request(&mut peer).await;
        respond_error_with_data(
            &mut peer,
            &wire_req,
            -32000,
            "this module was not granted model access",
            json!({"kind": "forbidden"}),
        )
        .await;
        let err: ModelFailure = call.await.unwrap().unwrap_err();
        assert_eq!(err, ModelFailure::Forbidden);
    }

    /// Positive control for the test above, at the other end of the
    /// mapping table: `cancelled` → `ModelFailure::Cancelled` through the
    /// `ModelPort` impl too (not just the inherent `complete`, covered by
    /// `model_client_cancelled_maps_through_the_real_call` above).
    #[tokio::test]
    async fn model_client_model_port_impl_maps_cancelled_to_model_failure_cancelled() {
        let (clients, mut peer) = fake_kernel(vec!["_a24/model/".to_string()]).await;
        let client = ModelClient::new(&clients).unwrap();
        let req = sample_request();
        let call = tokio::spawn(async move { ModelPort::complete(&client, req).await });

        let wire_req = read_request(&mut peer).await;
        respond_error_with_data(
            &mut peer,
            &wire_req,
            -32000,
            "the daemon is shutting down",
            json!({"kind": "cancelled"}),
        )
        .await;
        let err: ModelFailure = call.await.unwrap().unwrap_err();
        assert_eq!(err, ModelFailure::Cancelled);
    }
}
