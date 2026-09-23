//! `_a24/scheduler/{upsert,delete,list}` — typed client (T3.2.1).
//!
//! Wire shapes are copied field-for-field from the FROZEN design
//! (`Agent24-F1.1/docs/design/ME4-S1-scheduler-callback.md` §6.1) and its
//! SPEC mirror (`SPEC-ME3-OUT-OF-PROCESS.md` §3's method table) — this
//! module is the only place in Sin90 that knows those shapes; T3.3.2's
//! reconciler (the only production caller this round; T3.2.1 itself adds no
//! business caller) will see only this file's types.
//!
//! # `deny_unknown_fields`: request side yes, response side no
//!
//! The REQUEST types below (`ModuleSpec` as we send it, the params each
//! method builds) are `Serialize`-only — we control every byte we emit, so
//! there is nothing to "deny." The RESPONSE types (`ModuleScheduleState` and
//! friends) are deliberately **not** `deny_unknown_fields` on the
//! `Deserialize` side: the design doc's own struct is `deny_unknown_fields`
//! because IT is validating a hostile module's input, but here Sin90 is the
//! one being handed a kernel response — a kernel that adds a field to
//! `ModuleScheduleState` in a later release (e.g. a future `last_fire`
//! addition) must not crash every out-of-process module that has not been
//! rebuilt yet. Forward-compatible parsing, not strict validation, is the
//! right posture on this side of the wire.

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::error::{map_transport_error, ClientError};
use crate::adapter_agent24::KernelClients;

/// The prefix `Offer.provides` must cover for [`SchedulerClient::new`] to
/// return `Some` — matches `SIN90_CAPABILITY_PREFIXES`'s own entry and the
/// three methods' shared namespace (design §6.1).
pub const PREFIX: &str = "_a24/scheduler/";

/// What Sin90 SENDS as a schedule's `spec` — mirrors the design's
/// `ModuleSpec` (§6.1) exactly, including the `#[serde(tag = "type",
/// rename_all = "snake_case")]` shape the kernel's own `deny_unknown_fields`
/// enum expects. Also used to DESERIALIZE `ModuleScheduleState.spec` on the
/// way back (§6.1: "从不返回内核的 `schedule_id`", but it does echo `spec`
/// verbatim) — one type, both directions, since the shape is identical.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ModuleSpec {
    Cron {
        expr: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tz: Option<String>,
    },
    Every {
        secs: u32,
    },
    At {
        ts: String,
    },
}

/// One source's most recent fire (design §6.1's `LastFire`). Response-only —
/// no `deny_unknown_fields` (module docs).
#[derive(Debug, Clone, Deserialize)]
pub struct LastFire {
    pub fire_id: String,
    pub scheduled_for: String,
    /// `pending | deferred | delivered | failed | expired` — kept as a plain
    /// `String`, not a closed enum: this is diagnostic information a
    /// reconciler (T3.3.2) logs, and a new status value here must not fail
    /// to deserialize the whole response around it.
    pub status: String,
    pub last_error: Option<String>,
}

/// design §6.1's `LastFires` — the two delivery sources kept separate so
/// `run_now` never overwrites what happened to the last `tick` (SPEC's fired
/// section).
#[derive(Debug, Clone, Deserialize)]
pub struct LastFires {
    pub tick: Option<LastFire>,
    pub run_now: Option<LastFire>,
}

/// The kernel's full expected-state view of one schedule row (design §6.1's
/// `ModuleScheduleState`) — what `list` returns for every row, and what
/// `upsert` echoes back for the one it touched. `T3.3.2`'s reconciler
/// compares this, field for field, against Sin90's own `sin90_outbox` desired
/// state (spec.md M3).
#[derive(Debug, Clone, Deserialize)]
pub struct ModuleScheduleState {
    pub key: String,
    pub spec: ModuleSpec,
    pub enabled: bool,
    pub label: String,
    pub user_suspended: bool,
    pub system_disabled_reason: Option<String>,
    pub next_run_at: Option<String>,
    pub last_fire: LastFires,
}

/// `upsert`'s `outcome` (design §6.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UpsertOutcome {
    Created,
    Updated,
    Unchanged,
}

/// `delete`'s `outcome` — `Absent` is a SUCCESS, not an error (design §6.2:
/// "`rows_affected == 0` → `{outcome: "absent"}`，不是错误").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeleteOutcome {
    Deleted,
    Absent,
}

#[derive(Debug, Clone, Deserialize)]
pub struct UpsertResponse {
    pub outcome: UpsertOutcome,
    pub schedule: ModuleScheduleState,
}

#[derive(Debug, Clone, Deserialize)]
pub struct DeleteResponse {
    pub outcome: DeleteOutcome,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ListResponse {
    /// ≤ 256, sorted by key, never paginated (design §6.1).
    pub schedules: Vec<ModuleScheduleState>,
}

/// Typed `_a24/scheduler/*` client. Only [`SchedulerClient::new`] ever
/// constructs one — a `Some` return is the only proof `Offer.provides`
/// covered [`PREFIX`] (architecture.md 不可破边界 #7).
pub struct SchedulerClient {
    clients: Arc<KernelClients>,
}

impl SchedulerClient {
    /// `None` unless the handshake's `Offer` granted this prefix — callers
    /// (T3.3.2) hold an `Option<SchedulerClient>` and must handle the
    /// missing case themselves, never `.unwrap()` it into existence.
    #[must_use]
    pub fn new(clients: &Arc<KernelClients>) -> Option<Self> {
        clients.provides(PREFIX).then(|| Self {
            clients: Arc::clone(clients),
        })
    }

    /// `_a24/scheduler/upsert`. `label` is omitted from the wire params
    /// entirely when `None` — the kernel defaults it to `key` (design §6.1),
    /// so omitting is the correct way to say "use the default," not `null`.
    ///
    /// L3 (post-review correction): `enabled` is REQUIRED here, not
    /// `Option<bool>` the way `label`/`request_id` are. The kernel's own
    /// default (`enabled` absent → `true`, design §6.1's
    /// `SchedulerUpsertParams`) is an EXPECTED-STATE default, not a
    /// "leave whatever it currently is" default — there is no wire shape
    /// for "don't touch `enabled`" at all. An `Option<bool>` on this side
    /// would silently invite exactly the bug that distinction is meant to
    /// prevent: a caller passing `None` meaning "leave it paused" would
    /// actually RE-ENABLE a `Routine` the user deliberately suspended,
    /// because omitting the field on the wire means `true`, not "keep the
    /// paused state." T3.3.2's reconciler must always compute and pass the
    /// exact desired `enabled` value (from `sin90_outbox`'s `desired.enabled`,
    /// spec.md M3) — never `None`.
    ///
    /// `request_id`: H1's companion rule — pass `Some(id)` ONLY the id from
    /// the currently in-flight proxied request that is calling this (i.e.
    /// from inside a `POST /_a24/scheduler/fired` handler, echoing that
    /// request's own `X-A24-Request-Id`, T3.2.2). Any OTHER caller —
    /// T3.3.2's background reconciliation loop in particular — MUST pass
    /// `None`: a `request_id` that is not currently bound to a live proxied
    /// request gets `ClientError::RequestNotInFlight` (H1), not a background
    /// admission.
    pub async fn upsert(
        &self,
        key: &str,
        spec: &ModuleSpec,
        enabled: bool,
        label: Option<&str>,
        request_id: Option<&str>,
    ) -> Result<UpsertResponse, ClientError> {
        let mut params = json!({ "key": key, "spec": spec, "enabled": enabled });
        super::set_optional(&mut params, "label", label);
        super::set_optional(&mut params, "request_id", request_id);
        self.call("upsert", params).await
    }

    /// `_a24/scheduler/delete`. `{outcome: absent}` for a key this module
    /// never owned, or already deleted — see [`DeleteOutcome`]'s doc; that is
    /// success, not [`ClientError::NotFound`].
    ///
    /// `request_id`: same H1 rule as [`Self::upsert`] — only the current
    /// in-flight proxied request's id, `None` for background calls.
    pub async fn delete(
        &self,
        key: &str,
        request_id: Option<&str>,
    ) -> Result<DeleteResponse, ClientError> {
        let mut params = json!({ "key": key });
        super::set_optional(&mut params, "request_id", request_id);
        self.call("delete", params).await
    }

    /// `_a24/scheduler/list`. No pagination params (design §6.1: ≤ 256 rows,
    /// never paginated) — `request_id` is the only optional field, same H1
    /// rule as [`Self::upsert`]/[`Self::delete`]. In practice `list` is
    /// almost always the reconciler's own background full-account call
    /// (T3.3.2), so `None` is the common case here specifically.
    pub async fn list(&self, request_id: Option<&str>) -> Result<ListResponse, ClientError> {
        let mut params = json!({});
        super::set_optional(&mut params, "request_id", request_id);
        self.call("list", params).await
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
            ClientError::Other(format!(
                "scheduler/{method_suffix}: bad response shape: {e}"
            ))
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
        assert!(SchedulerClient::new(&clients).is_none());

        let (clients, _peer) = fake_kernel(vec!["_a24/scheduler/".to_string()]).await;
        assert!(SchedulerClient::new(&clients).is_some());
    }

    #[tokio::test]
    async fn upsert_request_shape_matches_design_6_1_field_for_field() {
        let (clients, mut peer) = fake_kernel(vec!["_a24/scheduler/".to_string()]).await;
        let client = SchedulerClient::new(&clients).unwrap();

        let spec = ModuleSpec::Cron {
            expr: "0 9 * * MON-FRI".to_string(),
            tz: Some("UTC".to_string()),
        };
        let call = tokio::spawn(async move {
            client
                .upsert(
                    "routine.abc",
                    &spec,
                    true,
                    Some("Morning run"),
                    Some("req-1"),
                )
                .await
        });

        let req = read_request(&mut peer).await;
        assert_eq!(req["method"], "_a24/scheduler/upsert");
        assert_eq!(
            req["params"],
            json!({
                "key": "routine.abc",
                "spec": {"type": "cron", "expr": "0 9 * * MON-FRI", "tz": "UTC"},
                "enabled": true,
                "label": "Morning run",
                "request_id": "req-1",
            })
        );

        respond(
            &mut peer,
            &req,
            json!({
                "outcome": "created",
                "schedule": {
                    "key": "routine.abc",
                    "spec": {"type": "cron", "expr": "0 9 * * MON-FRI", "tz": "UTC"},
                    "enabled": true,
                    "label": "Morning run",
                    "user_suspended": false,
                    "system_disabled_reason": null,
                    "next_run_at": "2030-01-01T09:00:00Z",
                    "last_fire": {"tick": null, "run_now": null}
                }
            }),
        )
        .await;

        let resp = call.await.unwrap().unwrap();
        assert_eq!(resp.outcome, UpsertOutcome::Created);
        assert_eq!(resp.schedule.key, "routine.abc");
    }

    #[tokio::test]
    async fn upsert_omits_absent_optional_fields_but_always_sends_enabled() {
        let (clients, mut peer) = fake_kernel(vec!["_a24/scheduler/".to_string()]).await;
        let client = SchedulerClient::new(&clients).unwrap();
        let spec = ModuleSpec::Every { secs: 3600 };
        tokio::spawn(async move { client.upsert("k", &spec, true, None, None).await });

        let req = read_request(&mut peer).await;
        assert_eq!(
            req["params"],
            json!({"key": "k", "spec": {"type": "every", "secs": 3600}, "enabled": true})
        );
    }

    /// L3: positive control for the test above and for the signature change
    /// itself — `enabled: false` must appear on the wire literally as
    /// `false`, never omitted the way `label`/`request_id` are. A caller
    /// pausing a `Routine` (`enabled: false`) must not accidentally leave the
    /// field off and re-enable it by the kernel's own "absent → true"
    /// default (design §6.1) — the whole point of L3 making this a required
    /// `bool` instead of `Option<bool>`.
    #[tokio::test]
    async fn upsert_enabled_false_is_sent_explicitly_not_omitted() {
        let (clients, mut peer) = fake_kernel(vec!["_a24/scheduler/".to_string()]).await;
        let client = SchedulerClient::new(&clients).unwrap();
        let spec = ModuleSpec::Every { secs: 3600 };
        tokio::spawn(async move { client.upsert("k", &spec, false, None, None).await });

        let req = read_request(&mut peer).await;
        assert_eq!(
            req["params"],
            json!({"key": "k", "spec": {"type": "every", "secs": 3600}, "enabled": false})
        );
    }

    #[tokio::test]
    async fn delete_absent_is_a_successful_outcome_not_an_error() {
        let (clients, mut peer) = fake_kernel(vec!["_a24/scheduler/".to_string()]).await;
        let client = SchedulerClient::new(&clients).unwrap();
        let call = tokio::spawn(async move { client.delete("k", None).await });

        let req = read_request(&mut peer).await;
        assert_eq!(req["method"], "_a24/scheduler/delete");
        assert_eq!(req["params"], json!({"key": "k"}));
        respond(&mut peer, &req, json!({"outcome": "absent"})).await;

        let resp = call.await.unwrap().unwrap();
        assert_eq!(resp.outcome, DeleteOutcome::Absent);
    }

    #[tokio::test]
    async fn list_request_has_no_params_beyond_optional_request_id() {
        let (clients, mut peer) = fake_kernel(vec!["_a24/scheduler/".to_string()]).await;
        let client = SchedulerClient::new(&clients).unwrap();
        let call = tokio::spawn(async move { client.list(None).await });

        let req = read_request(&mut peer).await;
        assert_eq!(req["method"], "_a24/scheduler/list");
        assert_eq!(req["params"], json!({}));
        respond(&mut peer, &req, json!({"schedules": []})).await;
        assert!(call.await.unwrap().unwrap().schedules.is_empty());
    }

    #[tokio::test]
    async fn forbidden_error_maps_through() {
        let (clients, mut peer) = fake_kernel(vec!["_a24/scheduler/".to_string()]).await;
        let client = SchedulerClient::new(&clients).unwrap();
        let call = tokio::spawn(async move { client.list(None).await });
        let req = read_request(&mut peer).await;
        respond_error(
            &mut peer,
            &req,
            -32000,
            "forbidden",
            "the `scheduler` capability was not granted",
        )
        .await;
        let err = call.await.unwrap().unwrap_err();
        assert!(matches!(err, ClientError::Forbidden(_)));
        assert!(err.is_permanent());
    }

    #[tokio::test]
    async fn rate_limited_error_maps_through() {
        let (clients, mut peer) = fake_kernel(vec!["_a24/scheduler/".to_string()]).await;
        let client = SchedulerClient::new(&clients).unwrap();
        let call = tokio::spawn(async move { client.list(None).await });
        let req = read_request(&mut peer).await;
        respond_error(
            &mut peer,
            &req,
            -32000,
            "rate_limited",
            "token bucket empty",
        )
        .await;
        let err = call.await.unwrap().unwrap_err();
        assert!(matches!(err, ClientError::RateLimited(_)));
        assert!(err.is_retryable());
    }

    #[tokio::test]
    async fn kernel_side_busy_maps_to_the_same_client_error_busy_as_this_ends_own_busy() {
        let (clients, mut peer) = fake_kernel(vec!["_a24/scheduler/".to_string()]).await;
        let client = SchedulerClient::new(&clients).unwrap();
        let call = tokio::spawn(async move { client.list(None).await });
        let req = read_request(&mut peer).await;
        respond_error(&mut peer, &req, -32000, "busy", "kernel busy").await;
        let err = call.await.unwrap().unwrap_err();
        assert!(matches!(err, ClientError::Busy(_)));
        assert!(err.is_retryable());
    }

    #[tokio::test]
    async fn invalid_params_error_maps_through() {
        let (clients, mut peer) = fake_kernel(vec!["_a24/scheduler/".to_string()]).await;
        let client = SchedulerClient::new(&clients).unwrap();
        let spec = ModuleSpec::Every { secs: 3600 };
        let call = tokio::spawn(async move { client.upsert("k", &spec, true, None, None).await });
        let req = read_request(&mut peer).await;
        respond_error(
            &mut peer,
            &req,
            -32602,
            "",
            "key contains an illegal character",
        )
        .await;
        let err = call.await.unwrap().unwrap_err();
        assert!(matches!(err, ClientError::InvalidParams(_)));
        assert!(err.is_permanent());
    }

    #[tokio::test]
    async fn disconnect_mid_call_yields_connection_lost_outcome_unknown() {
        let (clients, mut peer) = fake_kernel(vec!["_a24/scheduler/".to_string()]).await;
        let client = SchedulerClient::new(&clients).unwrap();
        let call = tokio::spawn(async move { client.list(None).await });
        let _req = read_request(&mut peer).await;
        drop(peer);

        let err = call.await.unwrap().unwrap_err();
        assert_eq!(err, ClientError::ConnectionLost);
        assert!(!err.is_permanent());
        assert!(!err.is_retryable());
    }
}
