//! `_a24/memory/private/{remember,recall,recent}` — typed client (T3.2.1).
//!
//! Wire shapes mirror Agent24's own `agent24d::memory_callback` handlers and
//! `agent24_domain::memory::{Remember, Remembered, Recollection}` (read
//! directly off that source this round — no frozen `ME4-S*` design doc covers
//! `private/*`, it shipped with ME-3d). Only `private/*` — `scoped/*` is not
//! offered this round (SPEC-ME3-OUT-OF-PROCESS.md §3: "取 (b)... 本轮只实现
//! `_a24/memory/private/*`"), so this client has no lease/scope parameter
//! anywhere; there is nowhere on the wire to put one.
//!
//! architecture.md's own contract table is explicit that this capability's
//! real job is narrow: "复盘定稿的**派生摘要**进内核私有记忆，供 AI 上下文
//! （真相仍在 Sin90 SQLite）" — this client is the mechanism, not a general
//! key-value store Sin90's business logic should reach for by default.
//!
//! Same `deny_unknown_fields` posture as [`super::scheduler`]: response types
//! here are plain `Deserialize` with no `deny_unknown_fields`, so a field the
//! kernel adds later does not break an unrebuilt Sin90.

use std::sync::Arc;

use serde::Deserialize;
use serde_json::{json, Map, Value};

use super::error::{map_transport_error, ClientError};
use crate::adapter_agent24::KernelClients;

/// The prefix `Offer.provides` must cover for [`MemoryClient::new`] to return
/// `Some`.
pub const PREFIX: &str = "_a24/memory/private/";

/// `remember`'s result (`agent24_domain::memory::Remembered`) — `id` is the
/// kernel-minted, deliberately opaque `osmem:<ULID>`; Sin90 must not parse it
/// (that crate's own doc comment), only round-trip it.
#[derive(Debug, Clone, Deserialize)]
pub struct Remembered {
    pub id: String,
    pub at: String,
}

/// One `recall`/`recent` hit (`agent24_domain::memory::Recollection`).
#[derive(Debug, Clone, Deserialize)]
pub struct Recollection {
    pub id: String,
    pub kind: String,
    pub body: Map<String, Value>,
    pub at: String,
}

/// `recall`/`recent`'s shared page shape (`agent24d::os_memory_page::RecallPage`).
#[derive(Debug, Clone, Deserialize)]
pub struct RecallPage {
    pub items: Vec<Recollection>,
    pub cursor: Option<String>,
}

/// Typed `_a24/memory/private/*` client. Only [`MemoryClient::new`] ever
/// constructs one.
pub struct MemoryClient {
    clients: Arc<KernelClients>,
}

impl MemoryClient {
    /// `None` unless the handshake's `Offer` granted [`PREFIX`]
    /// (architecture.md 不可破边界 #7).
    #[must_use]
    pub fn new(clients: &Arc<KernelClients>) -> Option<Self> {
        clients.provides(PREFIX).then(|| Self {
            clients: Arc::clone(clients),
        })
    }

    /// `_a24/memory/private/remember`. `kind` is Sin90's own free-form
    /// category (not the kernel's), `body` an arbitrary JSON object.
    ///
    /// M3 (post-review note): **not idempotent.** Every successful call
    /// mints a brand-new [`Remembered::id`] (`agent24_domain::memory`'s own
    /// doc: "the kernel mints the identifier") — there is no dedup key like
    /// scheduler's `key` or approval's `(module, request_id, kind)`. A
    /// blind retry after [`ClientError::Timeout`] or
    /// [`ClientError::ConnectionLost`] — both mean the outcome is genuinely
    /// UNKNOWN, not "definitely failed" (`ClientError::is_retryable`'s own
    /// doc) — risks writing the SAME memory twice under two different ids.
    /// Callers that need write-once semantics (the derived-summary use case
    /// architecture.md's contract table describes: "复盘定稿的**派生摘要**
    /// 进内核私有记忆") must supply their own idempotence above this client
    /// — e.g. checking `recall`/`recent` for an existing entry with the same
    /// `kind` and a caller-chosen marker in `body` before calling this, or
    /// simply accepting an occasional duplicate as an acceptable cost of a
    /// rare timeout (unlike scheduler's `upsert`, there is no wire-level fix
    /// available here — SPEC's `private/*` methods were not designed to be
    /// retried blindly).
    pub async fn remember(
        &self,
        kind: &str,
        body: Map<String, Value>,
        request_id: Option<&str>,
    ) -> Result<Remembered, ClientError> {
        let mut params = json!({ "kind": kind, "body": body });
        super::set_optional(&mut params, "request_id", request_id);
        self.call("remember", params).await
    }

    /// `_a24/memory/private/recall`.
    pub async fn recall(
        &self,
        query: &str,
        page_size: usize,
        cursor: Option<&str>,
        request_id: Option<&str>,
    ) -> Result<RecallPage, ClientError> {
        let mut params = json!({ "query": query, "page_size": page_size });
        super::set_optional(&mut params, "cursor", cursor);
        super::set_optional(&mut params, "request_id", request_id);
        self.call("recall", params).await
    }

    /// `_a24/memory/private/recent`.
    pub async fn recent(
        &self,
        page_size: usize,
        cursor: Option<&str>,
        request_id: Option<&str>,
    ) -> Result<RecallPage, ClientError> {
        let mut params = json!({ "page_size": page_size });
        super::set_optional(&mut params, "cursor", cursor);
        super::set_optional(&mut params, "request_id", request_id);
        self.call("recent", params).await
    }

    async fn call<T: serde::de::DeserializeOwned>(
        &self,
        method_suffix: &str,
        params: Value,
    ) -> Result<T, ClientError> {
        let value = self
            .clients
            .call(&format!("{PREFIX}{method_suffix}"), params)
            .await
            .map_err(map_transport_error)?;
        serde_json::from_value(value).map_err(|e| {
            ClientError::Other(format!("memory/{method_suffix}: bad response shape: {e}"))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapter_agent24::clients::test_support::{
        fake_kernel, read_request, respond, respond_error,
    };
    use serde_json::json;

    #[tokio::test]
    async fn offer_without_the_prefix_yields_none_with_the_prefix_yields_some() {
        let (clients, _peer) = fake_kernel(vec!["_a24/events/".to_string()]).await;
        assert!(MemoryClient::new(&clients).is_none());

        let (clients, _peer) = fake_kernel(vec!["_a24/memory/private/".to_string()]).await;
        assert!(MemoryClient::new(&clients).is_some());
    }

    #[tokio::test]
    async fn remember_request_shape_matches_the_kernel_handler_field_for_field() {
        let (clients, mut peer) = fake_kernel(vec!["_a24/memory/private/".to_string()]).await;
        let client = MemoryClient::new(&clients).unwrap();
        let mut body = Map::new();
        body.insert("text".to_string(), json!("weekly review draft"));
        let call =
            tokio::spawn(
                async move { client.remember("review.summary", body, Some("req-9")).await },
            );

        let req = read_request(&mut peer).await;
        assert_eq!(req["method"], "_a24/memory/private/remember");
        assert_eq!(
            req["params"],
            json!({"kind": "review.summary", "body": {"text": "weekly review draft"}, "request_id": "req-9"})
        );

        respond(
            &mut peer,
            &req,
            json!({"id": "osmem:01ABC", "at": "2030-01-01T00:00:00Z"}),
        )
        .await;
        let resp = call.await.unwrap().unwrap();
        assert_eq!(resp.id, "osmem:01ABC");
    }

    #[tokio::test]
    async fn recall_request_shape_and_response_page() {
        let (clients, mut peer) = fake_kernel(vec!["_a24/memory/private/".to_string()]).await;
        let client = MemoryClient::new(&clients).unwrap();
        let call = tokio::spawn(async move { client.recall("weekly", 10, None, None).await });

        let req = read_request(&mut peer).await;
        assert_eq!(req["method"], "_a24/memory/private/recall");
        assert_eq!(req["params"], json!({"query": "weekly", "page_size": 10}));

        respond(
            &mut peer,
            &req,
            json!({
                "items": [{"id": "osmem:1", "kind": "review.summary", "body": {}, "at": "t"}],
                "cursor": null
            }),
        )
        .await;
        let page = call.await.unwrap().unwrap();
        assert_eq!(page.items.len(), 1);
        assert!(page.cursor.is_none());
    }

    #[tokio::test]
    async fn recent_omits_absent_cursor() {
        let (clients, mut peer) = fake_kernel(vec!["_a24/memory/private/".to_string()]).await;
        let client = MemoryClient::new(&clients).unwrap();
        tokio::spawn(async move { client.recent(5, None, None).await });
        let req = read_request(&mut peer).await;
        assert_eq!(req["method"], "_a24/memory/private/recent");
        assert_eq!(req["params"], json!({"page_size": 5}));
    }

    #[tokio::test]
    async fn forbidden_error_maps_through() {
        let (clients, mut peer) = fake_kernel(vec!["_a24/memory/private/".to_string()]).await;
        let client = MemoryClient::new(&clients).unwrap();
        let call = tokio::spawn(async move { client.recent(5, None, None).await });
        let req = read_request(&mut peer).await;
        respond_error(
            &mut peer,
            &req,
            -32000,
            "forbidden",
            "this module was not granted a private memory handle",
        )
        .await;
        let err = call.await.unwrap().unwrap_err();
        assert!(matches!(err, ClientError::Forbidden(_)));
    }

    #[tokio::test]
    async fn rate_limited_error_maps_through() {
        let (clients, mut peer) = fake_kernel(vec!["_a24/memory/private/".to_string()]).await;
        let client = MemoryClient::new(&clients).unwrap();
        let call = tokio::spawn(async move { client.recent(5, None, None).await });
        let req = read_request(&mut peer).await;
        respond_error(&mut peer, &req, -32000, "rate_limited", "slow down").await;
        let err = call.await.unwrap().unwrap_err();
        assert!(matches!(err, ClientError::RateLimited(_)));
    }

    #[tokio::test]
    async fn busy_from_either_source_maps_the_same() {
        let (clients, mut peer) = fake_kernel(vec!["_a24/memory/private/".to_string()]).await;
        let client = MemoryClient::new(&clients).unwrap();
        let call = tokio::spawn(async move { client.recent(5, None, None).await });
        let req = read_request(&mut peer).await;
        respond_error(&mut peer, &req, -32000, "busy", "kernel busy").await;
        let err = call.await.unwrap().unwrap_err();
        assert!(matches!(err, ClientError::Busy(_)));
    }

    #[tokio::test]
    async fn invalid_params_error_maps_through() {
        let (clients, mut peer) = fake_kernel(vec!["_a24/memory/private/".to_string()]).await;
        let client = MemoryClient::new(&clients).unwrap();
        let call = tokio::spawn(async move { client.recall("q", 0, None, None).await });
        let req = read_request(&mut peer).await;
        respond_error(&mut peer, &req, -32602, "", "page_size must be > 0").await;
        let err = call.await.unwrap().unwrap_err();
        assert!(matches!(err, ClientError::InvalidParams(_)));
    }

    #[tokio::test]
    async fn disconnect_mid_call_yields_connection_lost() {
        let (clients, mut peer) = fake_kernel(vec!["_a24/memory/private/".to_string()]).await;
        let client = MemoryClient::new(&clients).unwrap();
        let call = tokio::spawn(async move { client.recent(5, None, None).await });
        let _req = read_request(&mut peer).await;
        drop(peer);
        let err = call.await.unwrap().unwrap_err();
        assert_eq!(err, ClientError::ConnectionLost);
    }
}
