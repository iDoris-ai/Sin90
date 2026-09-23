//! `_a24/approval/{gate,advise,status}` — typed client (T3.2.1).
//!
//! Wire shapes mirror Agent24's `agent24d::approval_callback` handlers and
//! `agent24_protocol::types::{ApprovalAnswer, ModuleApprovalKind,
//! ModuleApprovalDecision}` (T7b/ME-3e; SPEC-ME3-OUT-OF-PROCESS.md §3's
//! method table). `gate`/`advise` share one wire shape (only the method name
//! differs — `submit` below is the one place that builds it); `status` is a
//! separate query shape.
//!
//! `approval_token` is a one-time secret (kernel's own doc: "must never reach
//! a `Debug` output"). This client never logs `params` or any constructed
//! `Value` containing it — callers of [`ApprovalClient::gate`]/[`advise`]
//! should hold the same discipline with the token they pass in.

use std::sync::Arc;

use serde::Deserialize;
use serde_json::{json, Value};

use super::error::{map_transport_error, ClientError};
use crate::adapter_agent24::KernelClients;

/// The prefix `Offer.provides` must cover for [`ApprovalClient::new`] to
/// return `Some`.
pub const PREFIX: &str = "_a24/approval/";

/// Mirrors `agent24_protocol::types::ModuleApprovalKind`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModuleApprovalKind {
    Gate,
    Advise,
}

/// Mirrors `agent24_protocol::types::ModuleApprovalDecision`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModuleApprovalDecision {
    Pending,
    Approved,
    Denied,
    TimedOut,
}

/// Mirrors `agent24_protocol::types::ApprovalAnswer` — the shape all three
/// methods return (`gate`/`advise` return it fresh at `Pending`; `status`
/// returns the current value).
#[derive(Debug, Clone, Deserialize)]
pub struct ApprovalAnswer {
    pub approval_id: String,
    pub kind: ModuleApprovalKind,
    pub binding: bool,
    pub decision: ModuleApprovalDecision,
    pub executed_at: Option<String>,
}

/// Typed `_a24/approval/*` client. Only [`ApprovalClient::new`] ever
/// constructs one.
pub struct ApprovalClient {
    clients: Arc<KernelClients>,
}

impl ApprovalClient {
    /// `None` unless the handshake's `Offer` granted [`PREFIX`]
    /// (architecture.md 不可破边界 #7).
    #[must_use]
    pub fn new(clients: &Arc<KernelClients>) -> Option<Self> {
        clients.provides(PREFIX).then(|| Self {
            clients: Arc::clone(clients),
        })
    }

    /// `_a24/approval/gate` — submits a KERNEL-EXECUTED action for approval.
    /// T7c/ME-3e: the closed set of executable actions may be empty or
    /// narrow; an action outside it comes back [`ClientError::Forbidden`]
    /// (SPEC's own wording: "reusing `Forbidden`, not a more precise kind").
    pub async fn gate(
        &self,
        action: &str,
        target: Option<&str>,
        payload: Value,
        request_id: &str,
        approval_token: &str,
    ) -> Result<ApprovalAnswer, ClientError> {
        self.submit("gate", action, target, payload, request_id, approval_token)
            .await
    }

    /// `_a24/approval/advise` — submits a MODULE-DOMAIN action for
    /// knowledge/record only; the kernel does not execute it and does not
    /// guarantee it is honored (SPEC §6.1).
    pub async fn advise(
        &self,
        action: &str,
        target: Option<&str>,
        payload: Value,
        request_id: &str,
        approval_token: &str,
    ) -> Result<ApprovalAnswer, ClientError> {
        self.submit(
            "advise",
            action,
            target,
            payload,
            request_id,
            approval_token,
        )
        .await
    }

    /// `_a24/approval/status` — an independent read, any number of times,
    /// regardless of whether the originating request is still alive (Agent24
    /// `ApprovalStatusHandler`'s own doc comment).
    pub async fn status(&self, approval_id: &str) -> Result<ApprovalAnswer, ClientError> {
        let params = json!({ "approval_id": approval_id });
        self.call("status", params).await
    }

    async fn submit(
        &self,
        method_suffix: &str,
        action: &str,
        target: Option<&str>,
        payload: Value,
        request_id: &str,
        approval_token: &str,
    ) -> Result<ApprovalAnswer, ClientError> {
        let mut params = json!({
            "action": action,
            "payload": payload,
            "request_id": request_id,
            "approval_token": approval_token,
        });
        super::set_optional(&mut params, "target", target);
        self.call(method_suffix, params).await
    }

    async fn call(
        &self,
        method_suffix: &str,
        params: Value,
    ) -> Result<ApprovalAnswer, ClientError> {
        let value = self
            .clients
            .call(&format!("{PREFIX}{method_suffix}"), params)
            .await
            .map_err(map_transport_error)?;
        serde_json::from_value(value).map_err(|e| {
            ClientError::Other(format!("approval/{method_suffix}: bad response shape: {e}"))
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
        assert!(ApprovalClient::new(&clients).is_none());

        let (clients, _peer) = fake_kernel(vec!["_a24/approval/".to_string()]).await;
        assert!(ApprovalClient::new(&clients).is_some());
    }

    #[tokio::test]
    async fn gate_request_shape_matches_the_kernel_handler_field_for_field() {
        let (clients, mut peer) = fake_kernel(vec!["_a24/approval/".to_string()]).await;
        let client = ApprovalClient::new(&clients).unwrap();
        let call = tokio::spawn(async move {
            client
                .gate(
                    "schedule_callback",
                    Some("2030-01-01T09:00:00Z"),
                    json!({"note": "run it"}),
                    "req-1",
                    "tok-secret",
                )
                .await
        });

        let req = read_request(&mut peer).await;
        assert_eq!(req["method"], "_a24/approval/gate");
        assert_eq!(
            req["params"],
            json!({
                "action": "schedule_callback",
                "target": "2030-01-01T09:00:00Z",
                "payload": {"note": "run it"},
                "request_id": "req-1",
                "approval_token": "tok-secret",
            })
        );

        respond(
            &mut peer,
            &req,
            json!({
                "approval_id": "appr-1",
                "kind": "gate",
                "binding": true,
                "decision": "pending",
                "executed_at": null
            }),
        )
        .await;
        let resp = call.await.unwrap().unwrap();
        assert_eq!(resp.kind, ModuleApprovalKind::Gate);
        assert_eq!(resp.decision, ModuleApprovalDecision::Pending);
        assert!(resp.binding);
    }

    #[tokio::test]
    async fn advise_omits_absent_target() {
        let (clients, mut peer) = fake_kernel(vec!["_a24/approval/".to_string()]).await;
        let client = ApprovalClient::new(&clients).unwrap();
        tokio::spawn(async move {
            client
                .advise("routine.note", None, json!({}), "req-2", "tok")
                .await
        });
        let req = read_request(&mut peer).await;
        assert_eq!(req["method"], "_a24/approval/advise");
        assert_eq!(
            req["params"],
            json!({"action": "routine.note", "payload": {}, "request_id": "req-2", "approval_token": "tok"})
        );
    }

    #[tokio::test]
    async fn status_request_shape() {
        let (clients, mut peer) = fake_kernel(vec!["_a24/approval/".to_string()]).await;
        let client = ApprovalClient::new(&clients).unwrap();
        let call = tokio::spawn(async move { client.status("appr-1").await });
        let req = read_request(&mut peer).await;
        assert_eq!(req["method"], "_a24/approval/status");
        assert_eq!(req["params"], json!({"approval_id": "appr-1"}));
        respond(
            &mut peer,
            &req,
            json!({
                "approval_id": "appr-1",
                "kind": "advise",
                "binding": false,
                "decision": "approved",
                "executed_at": null
            }),
        )
        .await;
        let resp = call.await.unwrap().unwrap();
        assert_eq!(resp.decision, ModuleApprovalDecision::Approved);
    }

    #[tokio::test]
    async fn forbidden_error_maps_through() {
        let (clients, mut peer) = fake_kernel(vec!["_a24/approval/".to_string()]).await;
        let client = ApprovalClient::new(&clients).unwrap();
        let call = tokio::spawn(async move { client.status("nope").await });
        let req = read_request(&mut peer).await;
        respond_error(&mut peer, &req, -32000, "forbidden", "no grant").await;
        let err = call.await.unwrap().unwrap_err();
        assert!(matches!(err, ClientError::Forbidden(_)));
    }

    #[tokio::test]
    async fn not_found_error_maps_through() {
        let (clients, mut peer) = fake_kernel(vec!["_a24/approval/".to_string()]).await;
        let client = ApprovalClient::new(&clients).unwrap();
        let call = tokio::spawn(async move { client.status("nope").await });
        let req = read_request(&mut peer).await;
        respond_error(&mut peer, &req, -32000, "not_found", "approval not found").await;
        let err = call.await.unwrap().unwrap_err();
        assert!(matches!(err, ClientError::NotFound(_)));
        assert!(!err.is_permanent());
        assert!(!err.is_retryable());
    }

    #[tokio::test]
    async fn busy_from_either_source_maps_the_same() {
        let (clients, mut peer) = fake_kernel(vec!["_a24/approval/".to_string()]).await;
        let client = ApprovalClient::new(&clients).unwrap();
        let call = tokio::spawn(async move { client.status("x").await });
        let req = read_request(&mut peer).await;
        respond_error(&mut peer, &req, -32000, "busy", "kernel busy").await;
        let err = call.await.unwrap().unwrap_err();
        assert!(matches!(err, ClientError::Busy(_)));
    }

    #[tokio::test]
    async fn invalid_params_error_maps_through() {
        let (clients, mut peer) = fake_kernel(vec!["_a24/approval/".to_string()]).await;
        let client = ApprovalClient::new(&clients).unwrap();
        let call = tokio::spawn(async move {
            client
                .gate("schedule_callback", None, json!({}), "r", "t")
                .await
        });
        let req = read_request(&mut peer).await;
        respond_error(&mut peer, &req, -32602, "", "missing target").await;
        let err = call.await.unwrap().unwrap_err();
        assert!(matches!(err, ClientError::InvalidParams(_)));
    }

    #[tokio::test]
    async fn disconnect_mid_call_yields_connection_lost() {
        let (clients, mut peer) = fake_kernel(vec!["_a24/approval/".to_string()]).await;
        let client = ApprovalClient::new(&clients).unwrap();
        let call = tokio::spawn(async move { client.status("x").await });
        let _req = read_request(&mut peer).await;
        drop(peer);
        let err = call.await.unwrap().unwrap_err();
        assert_eq!(err, ClientError::ConnectionLost);
    }
}
