//! TS.1.0 (ME4-S3-os-sdk.md §5.1) — wire golden samples, captured on the
//! PRE-migration `adapter_agent24` implementation (this crate's own
//! hand-written transport/clients), before ME4-5.2.1 replaces it with
//! `agent24-os-sdk`. Two directions:
//!
//! - *Outbound*: the exact `(method, params)` JSON-RPC requests Sin90 sends,
//!   for the call shapes §5.1's table lists — scheduler `upsert`
//!   (cron+tz)/`delete`/`list`, a `memory.remember` preceded by its `recall`
//!   dedup pre-check, one `_a24/model/complete` per capability
//!   (classify/summarize/propose), a Routine-change `_a24/events/emit`, and
//!   the handshake `initialize` params (minus `auth_token`, which is a
//!   per-process secret, not a wire *shape* fact worth pinning byte-for-byte).
//! - *Inbound*: representative kernel error replies, captured at the
//!   `ClientError` boundary (the variant + `is_permanent()`/`is_retryable()`),
//!   which is exactly the boundary `clients/error.rs` (pre-migration) and
//!   `agent24_os_sdk::ClientError` (post-migration) both promise to keep
//!   identical — see this module's own doc note below on why the FULL
//!   18-kind × outbox-row-status matrix the design's §5.1 sketches is not
//!   independently re-captured here.
//!
//! # Deviation from the frozen design's §5.1 sketch (recorded here, not
//! silently narrowed)
//!
//! §5.1 describes recording, per kernel error kind, the resulting
//! `(row.status, row.attempts, row.other_bucket_attempts, backoff tier, pump
//! action)` via `reconciler::apply_one`. This crate's OWN pre-existing
//! `adapter_agent24::reconciler` test module already exercises that exact
//! surface exhaustively and precisely — e.g.
//! `reconcile_m2_recall_precheck_exhausting_all_pages_retries_instead_of_assuming_absent`
//! (Inconclusive -> pending, `other_bucket_attempts == 1`),
//! `reconcile_m3_remember_response_lost_then_recall_finds_it_remember_called_once_total`
//! (ConnectionLost -> row untouched, `attempts == 0`, then `remember` called
//! at most once total), and `clients::error::tests::
//! classification_matches_spec_md_m3_exactly` / `every_spec_error_kind_maps_to_its_documented_variant`
//! (all 18 kernel `kind`s -> `ClientError` variant -> `is_permanent`/
//! `is_retryable`). ME4-5.2.1's own acceptance rule (§5.2 point 3, H4/H-A)
//! requires that test module to survive the SDK migration **byte-identical**
//! (reconciler.rs) or changed by exactly one designated `use` line
//! (model.rs) — which makes "run unchanged, still green" a STRICTER
//! zero-behavior-change proof for that surface than a second, hand-rolled
//! JSON-fixture harness duplicating the same scenarios would be. Building a
//! parallel golden-JSON version of that same matrix here would not add
//! coverage; it would just be a second copy of the same assertions,
//! disallowed from ever being exercised again post-migration (since
//! `reconciler.rs`'s test module itself must not change). This file's own
//! inbound section therefore captures the ONE thing those existing tests do
//! NOT already pin as a byte-level fixture: a representative cross-section
//! of `ClientError` variants captured directly off the wire client boundary
//! (`SchedulerClient`/`ModelClient`), as a belt-and-suspenders check on top
//! of `clients/error.rs`'s own exhaustive unit tests (kept for the same
//! reason: they, too, live inside a `#[cfg(test)] mod tests` block this
//! crate does not carry across the migration, since `clients/error.rs`
//! itself is deleted in favor of `agent24_os_sdk::ClientError` — see
//! `docs/design/ME4-S3-os-sdk.md` §5.1's `clients/{error,memory,approval,mod}.rs`
//! row).
//!
//! # Recording vs. checking
//!
//! Run with `SIN90_GOLDEN_RECORD=1 cargo test --test ... ts10_` (or just
//! `cargo test ts10_` inside this crate) once, on this PRE-migration branch,
//! to (re)write the fixtures under `tests/golden/`. Every subsequent run (on
//! this branch AND on the post-migration branch) checks the current
//! implementation against the already-recorded fixtures.

#![cfg(test)]

use std::path::PathBuf;

use serde_json::{json, Map, Value};

use super::clients::model::ModelClient;
use super::clients::scheduler::ModuleSpec;
use super::clients::test_support::{
    fake_kernel, read_request, respond, respond_error, respond_error_with_data,
};
use super::clients::{ClientError, MemoryClient, SchedulerClient};
use super::KernelClients;
use crate::ai::classify::{build_classify_request, KeyedCandidate};
use crate::ai::ports::Engine;
use crate::ai::propose::{build_propose_request, KeyedDirection, KeyedTask};
use crate::ai::summarize::{build_summarize_request, Fact};
use crate::core::types::{Energy, Task, TaskKind, TaskStatus};

fn golden_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/golden")
}

/// TS.1.0/TS.1.1's own comparison rule (§5.1): "比较时忽略 JSON-RPC `id`
/// 值与对象键序". `serde_json::Value`'s `Map` is a `BTreeMap` in this crate
/// (no `preserve_order` feature — `Cargo.toml`), so object key order is
/// already a non-issue for `==`; only `id` needs stripping by hand.
fn strip_id(mut v: Value) -> Value {
    if let Some(obj) = v.as_object_mut() {
        obj.remove("id");
    }
    v
}

/// A stable classification tag for a [`ClientError`] — the variant name (and,
/// for `Unavailable`, its `cause`), deliberately WITHOUT the free-text
/// message. `ClientError::Busy`/etc. carry `info.message`/`info.to_string()`
/// verbatim (`clients/error.rs`'s own `map_rpc_error`), and that FORMATTING
/// is not itself a wire-shape fact worth pinning: `agent24_os_sdk::
/// ClientError`'s equivalent mapping (`error.rs`'s `From<CallError>`) uses
/// the kernel's raw `data.message` directly rather than Sin90's
/// `"{kind} ({code}): {message}"` `Display` string, so the exact text
/// legitimately differs post-migration even though the variant, `cause`,
/// `is_permanent()`, and `is_retryable()` — the only things any caller in
/// this crate ever matches on — do not. No wildcard arm: a future
/// `ClientError` variant this function forgets to tag fails to compile,
/// mirroring `clients/error.rs`'s own posture.
fn client_error_tag(err: &ClientError) -> String {
    match err {
        ClientError::Forbidden(_) => "forbidden".to_string(),
        ClientError::RateLimited(_) => "rate_limited".to_string(),
        ClientError::Busy(_) => "busy".to_string(),
        ClientError::QuotaExceeded(_) => "quota_exceeded".to_string(),
        ClientError::InvalidParams(_) => "invalid_params".to_string(),
        ClientError::Timeout(_) => "timeout".to_string(),
        ClientError::RequestNotInFlight(_) => "request_not_in_flight".to_string(),
        ClientError::NotReady(_) => "not_ready".to_string(),
        ClientError::Draining(_) => "draining".to_string(),
        ClientError::Revoked(_) => "revoked".to_string(),
        ClientError::TokenInvalid(_) => "token_invalid".to_string(),
        ClientError::PayloadTooLarge(_) => "payload_too_large".to_string(),
        ClientError::NotFound(_) => "not_found".to_string(),
        ClientError::ConnectionLost => "connection_lost".to_string(),
        ClientError::NotSent(_) => "not_sent".to_string(),
        ClientError::Other(_) => "other".to_string(),
        ClientError::Unavailable { cause, .. } => format!("unavailable:{cause:?}"),
        ClientError::Cancelled => "cancelled".to_string(),
    }
}

/// Compares `got` against the fixture `tests/golden/<name>.json`, or
/// (re)records it when `SIN90_GOLDEN_RECORD=1` is set in the environment.
fn assert_golden(name: &str, got: &Value) {
    let path = golden_dir().join(format!("{name}.json"));
    if std::env::var("SIN90_GOLDEN_RECORD").as_deref() == Ok("1") {
        std::fs::create_dir_all(golden_dir()).unwrap();
        std::fs::write(
            &path,
            format!("{}\n", serde_json::to_string_pretty(got).unwrap()),
        )
        .unwrap_or_else(|e| panic!("writing golden fixture {path:?}: {e}"));
        return;
    }
    let existing = std::fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!(
            "golden fixture {path:?} missing or unreadable ({e}) — run with \
             SIN90_GOLDEN_RECORD=1 to (re)record it on the pre-migration branch first"
        )
    });
    let want: Value = serde_json::from_str(&existing)
        .unwrap_or_else(|e| panic!("golden fixture {path:?} is not valid JSON: {e}"));
    assert_eq!(
        &want, got,
        "golden mismatch for {name:?} (fixture: {path:?})"
    );
}

// --------------------------------------------------------------- outbound --

#[tokio::test]
async fn ts10_out_scheduler_upsert_cron_tz() {
    let (clients, mut peer) = fake_kernel(vec!["_a24/scheduler/".to_string()]).await;
    let scheduler = SchedulerClient::new(&clients).unwrap();
    let spec = ModuleSpec::Cron {
        expr: "0 7 * * MON,WED,FRI".to_string(),
        tz: Some("Asia/Shanghai".to_string()),
    };
    let call = tokio::spawn(async move {
        scheduler
            .upsert("routine.abc123def456", &spec, true, None, None)
            .await
    });
    let req = read_request(&mut peer).await;
    assert_eq!(req["method"], "_a24/scheduler/upsert");
    assert_golden("ts10_out_scheduler_upsert", &strip_id(req.clone()));
    respond(
        &mut peer,
        &req,
        json!({
            "outcome": "created",
            "schedule": {
                "key": "routine.abc123def456",
                "spec": {"type": "cron", "expr": "0 7 * * MON,WED,FRI", "tz": "Asia/Shanghai"},
                "enabled": true,
                "label": "routine.abc123def456",
                "user_suspended": false,
                "system_disabled_reason": null,
                "next_run_at": null,
                "last_fire": {"tick": null, "run_now": null},
            }
        }),
    )
    .await;
    call.await.unwrap().unwrap();
}

#[tokio::test]
async fn ts10_out_scheduler_delete() {
    let (clients, mut peer) = fake_kernel(vec!["_a24/scheduler/".to_string()]).await;
    let scheduler = SchedulerClient::new(&clients).unwrap();
    let call = tokio::spawn(async move { scheduler.delete("routine.abc123def456", None).await });
    let req = read_request(&mut peer).await;
    assert_eq!(req["method"], "_a24/scheduler/delete");
    assert_golden("ts10_out_scheduler_delete", &strip_id(req.clone()));
    respond(&mut peer, &req, json!({"outcome": "deleted"})).await;
    call.await.unwrap().unwrap();
}

#[tokio::test]
async fn ts10_out_scheduler_list() {
    let (clients, mut peer) = fake_kernel(vec!["_a24/scheduler/".to_string()]).await;
    let scheduler = SchedulerClient::new(&clients).unwrap();
    let call = tokio::spawn(async move { scheduler.list(None).await });
    let req = read_request(&mut peer).await;
    assert_eq!(req["method"], "_a24/scheduler/list");
    assert_golden("ts10_out_scheduler_list", &strip_id(req.clone()));
    respond(&mut peer, &req, json!({"schedules": []})).await;
    call.await.unwrap().unwrap();
}

/// `memory.remember` preceded by its `recall` dedup pre-check (§5.1's own
/// wording: "`memory.remember` 前的 recall 翻页 + remember（含
/// `body.dedup_key`）") — one page, no match, then `remember`. The
/// multi-page/`Inconclusive` shapes of this same algorithm are already
/// pinned behaviorally by `reconciler.rs`'s own pre-existing tests (this
/// module's top-level doc); this golden only pins the two calls' wire
/// bytes.
#[tokio::test]
async fn ts10_out_memory_recall_then_remember() {
    let (clients, mut peer) = fake_kernel(vec!["_a24/memory/private/".to_string()]).await;
    let memory = MemoryClient::new(&clients).unwrap();
    let dedup_key = "review:weekly:2026-W39";
    let memory_for_call = memory.clone();
    let call = tokio::spawn(async move {
        let page = memory_for_call.recall(dedup_key, 50, None, None).await?;
        assert!(page.cursor.is_none());
        assert!(page.items.is_empty());
        let mut body = Map::new();
        body.insert("dedup_key".to_string(), json!(dedup_key));
        body.insert("review_id".to_string(), json!("rev-1"));
        body.insert("summary".to_string(), json!("Shipped the SDK migration."));
        memory_for_call.remember("review.summary", body, None).await
    });

    let recall_req = read_request(&mut peer).await;
    assert_eq!(recall_req["method"], "_a24/memory/private/recall");
    assert_golden("ts10_out_memory_recall", &strip_id(recall_req.clone()));
    respond(&mut peer, &recall_req, json!({"items": [], "cursor": null})).await;

    let remember_req = read_request(&mut peer).await;
    assert_eq!(remember_req["method"], "_a24/memory/private/remember");
    assert_golden("ts10_out_memory_remember", &strip_id(remember_req.clone()));
    respond(
        &mut peer,
        &remember_req,
        json!({"id": "osmem:01JXAMPLE", "at": "2026-09-29T00:00:00Z"}),
    )
    .await;

    let remembered = call.await.unwrap().unwrap();
    assert_eq!(remembered.id, "osmem:01JXAMPLE");
}

fn fixture_task() -> Task {
    Task {
        id: "tsk_01JXAMPLE".to_string(),
        direction_id: Some("dir_focus".to_string()),
        week_id: Some("wk_2026w39".to_string()),
        parent_task_id: None,
        title: "Write the Q3 rollup memo".to_string(),
        status: TaskStatus::Backlog,
        kind: TaskKind::DeepWork,
        energy: Energy::High,
        est_minutes: Some(90),
        carried_from: None,
        created_at: "2026-09-28T00:00:00Z".to_string(),
        updated_at: "2026-09-28T00:00:00Z".to_string(),
    }
}

#[tokio::test]
async fn ts10_out_model_complete_classify() {
    let (clients, mut peer) = fake_kernel(vec!["_a24/model/".to_string()]).await;
    let model = ModelClient::new(&clients).unwrap();
    let task = fixture_task();
    let candidates = vec![
        KeyedCandidate {
            key: "c1".to_string(),
            direction_id: "dir_focus".to_string(),
            title: "Deep Focus".to_string(),
            area_title: Some("Work".to_string()),
        },
        KeyedCandidate {
            key: "c2".to_string(),
            direction_id: "dir_admin".to_string(),
            title: "Admin".to_string(),
            area_title: None,
        },
    ];
    let req = build_classify_request(&task, &candidates, Engine::Local);
    let call = tokio::spawn(async move { model.complete(&req).await });

    let wire_req = read_request(&mut peer).await;
    assert_eq!(wire_req["method"], "_a24/model/complete");
    assert_golden(
        "ts10_out_model_complete_classify",
        &strip_id(wire_req.clone()),
    );
    respond(
        &mut peer,
        &wire_req,
        json!({
            "text": "{\"choice\":\"c1\",\"confidence\":\"high\",\"reason\":\"matches\"}",
            "model_id": "test-model",
            "tier": "local",
            "usage": {"prompt_tokens": 120, "completion_tokens": 30},
        }),
    )
    .await;
    call.await.unwrap().unwrap();
}

#[tokio::test]
async fn ts10_out_model_complete_summarize() {
    let (clients, mut peer) = fake_kernel(vec!["_a24/model/".to_string()]).await;
    let model = ModelClient::new(&clients).unwrap();
    let facts = vec![Fact {
        key: "f1".to_string(),
        label: "Tasks done".to_string(),
        value: "5".to_string(),
    }];
    let titles = vec!["Write the Q3 rollup memo".to_string()];
    let req = build_summarize_request(&facts, &titles, Engine::Local);
    let call = tokio::spawn(async move { model.complete(&req).await });

    let wire_req = read_request(&mut peer).await;
    assert_eq!(wire_req["method"], "_a24/model/complete");
    assert_golden(
        "ts10_out_model_complete_summarize",
        &strip_id(wire_req.clone()),
    );
    respond(
        &mut peer,
        &wire_req,
        json!({
            "text": "{\"narrative\":\"A steady week.\"}",
            "model_id": "test-model",
            "tier": "local",
            "usage": {"prompt_tokens": 200, "completion_tokens": 40},
        }),
    )
    .await;
    call.await.unwrap().unwrap();
}

#[tokio::test]
async fn ts10_out_model_complete_propose() {
    let (clients, mut peer) = fake_kernel(vec!["_a24/model/".to_string()]).await;
    let model = ModelClient::new(&clients).unwrap();
    let p = vec![KeyedTask {
        key: "p1".to_string(),
        task_id: "tsk_carry".to_string(),
        title: "Carryover task".to_string(),
    }];
    let w = vec![KeyedTask {
        key: "w1".to_string(),
        task_id: "tsk_reorder".to_string(),
        title: "Reorder task".to_string(),
    }];
    let g = vec![KeyedDirection {
        key: "g1".to_string(),
        direction_id: "dir_focus".to_string(),
        title: "Deep Focus".to_string(),
    }];
    let req = build_propose_request(&p, &w, &g, Engine::Executive);
    let call = tokio::spawn(async move { model.complete(&req).await });

    let wire_req = read_request(&mut peer).await;
    assert_eq!(wire_req["method"], "_a24/model/complete");
    assert_golden(
        "ts10_out_model_complete_propose",
        &strip_id(wire_req.clone()),
    );
    respond(
        &mut peer,
        &wire_req,
        json!({
            "text": "{\"carry\":[],\"order\":[],\"new_tasks\":[],\"reason\":\"ok\"}",
            "model_id": "test-model",
            "tier": "remote",
            "usage": {"prompt_tokens": 300, "completion_tokens": 60},
        }),
    )
    .await;
    call.await.unwrap().unwrap();
}

/// A Routine-change `_a24/events/emit` (§5.1: "events：一个 Routine 变更触发
/// 的 emit") — the exact `kind`/payload shape `http::transition_routine`
/// sends on a pause transition (`http/mod.rs`'s own doc comment on that
/// handler). Captured at `KernelClients::call` directly (what
/// `KernelEventSink`'s worker ultimately sends) rather than round-tripping
/// through the bounded mpsc queue and worker pool, which would make this
/// test's timing depend on tokio's scheduler for no wire-shape benefit.
#[tokio::test]
async fn ts10_out_events_emit_routine_paused() {
    let (clients, mut peer) = fake_kernel(vec!["_a24/events/".to_string()]).await;
    let call = tokio::spawn(async move {
        clients
            .call(
                "_a24/events/emit",
                json!({"kind": "routine.paused", "payload": {"routine_id": "rtn_01JXAMPLE"}}),
            )
            .await
    });
    let req = read_request(&mut peer).await;
    assert_eq!(req["method"], "_a24/events/emit");
    assert_golden(
        "ts10_out_events_emit_routine_paused",
        &strip_id(req.clone()),
    );
    respond(&mut peer, &req, json!({})).await;
    call.await.unwrap().unwrap();
}

/// Handshake `initialize` params, minus `auth_token` (§5.1: "握手：
/// `initialize` 的 params（去掉 `auth_token`）") — `auth_token` is a
/// per-process secret, not a wire *shape* fact, so it is excluded from the
/// fixture rather than pinned to a specific literal value.
#[tokio::test]
async fn ts10_out_handshake_initialize_params() {
    use serde_json::Value as V;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    let dir = std::env::temp_dir().join(format!("sin90-golden-test-{}", crate::core::ulid()));
    std::fs::create_dir_all(&dir).unwrap();
    let sock_path = dir.join("cb.sock");
    let listener = tokio::net::UnixListener::bind(&sock_path).unwrap();
    let manifest = b"name: sin90\n";
    let token = "test-token-not-part-of-the-golden";

    let server = tokio::spawn({
        let manifest = manifest.to_vec();
        async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut reader = BufReader::new(stream);
            let mut buf = Vec::new();
            reader.read_until(b'\n', &mut buf).await.unwrap();
            let mut req: V = serde_json::from_slice(&buf).unwrap();
            assert_eq!(req["method"], "initialize");
            assert_eq!(
                req["params"]["manifest_digest"],
                super::manifest_digest(&manifest)
            );
            if let Some(params) = req.get_mut("params").and_then(|p| p.as_object_mut()) {
                params.remove("auth_token");
            }
            assert_golden("ts10_out_handshake_initialize", &strip_id(req.clone()));
            let resp = json!({
                "jsonrpc": "2.0", "id": req["id"],
                "result": {"protocol_version": 1, "offer": {"provides": []}}
            });
            let mut bytes = serde_json::to_vec(&resp).unwrap();
            bytes.push(b'\n');
            reader.get_mut().write_all(&bytes).await.unwrap();
        }
    });

    let noop_hook: super::FatalHook = std::sync::Arc::new(|| {});
    let (_clients, _offer) =
        KernelClients::handshake(&sock_path, "sin90", manifest, token, noop_hook)
            .await
            .unwrap();
    server.await.unwrap();
}

// ---------------------------------------------------------------- inbound --

/// A representative cross-section of kernel error kinds, captured at the
/// `ClientError` boundary — see this module's top-level doc for why the
/// full outbox-row-level matrix is not independently re-captured here.
#[tokio::test]
async fn ts10_in_scheduler_upsert_error_kinds() {
    let cases: &[(i64, &str, &str)] = &[
        (-32602, "", "invalid_params_code"),
        (-32000, "forbidden", "forbidden"),
        (-32000, "busy", "busy"),
        (-32000, "quota_exceeded", "quota_exceeded"),
        (-32000, "not_ready", "not_ready"),
        (-32000, "draining", "draining"),
        (-32000, "revoked", "revoked"),
        (-32000, "rate_limited", "rate_limited"),
        (-32000, "payload_too_large", "payload_too_large"),
        (-32000, "token_invalid", "token_invalid"),
        (-32000, "not_found", "not_found"),
        (-32000, "cancelled", "cancelled"),
        (-32000, "timeout", "timeout"),
    ];
    let mut got = Map::new();
    for (code, kind, label) in cases.iter().copied() {
        let (clients, mut peer) = fake_kernel(vec!["_a24/scheduler/".to_string()]).await;
        let scheduler = SchedulerClient::new(&clients).unwrap();
        let spec = ModuleSpec::Cron {
            expr: "0 7 * * *".to_string(),
            tz: Some("UTC".to_string()),
        };
        let call =
            tokio::spawn(
                async move { scheduler.upsert("routine.x", &spec, true, None, None).await },
            );
        let req = read_request(&mut peer).await;
        respond_error(&mut peer, &req, code, kind, "test-injected").await;
        let err = call.await.unwrap().unwrap_err();
        got.insert(
            label.to_string(),
            json!({
                "tag": client_error_tag(&err),
                "is_permanent": err.is_permanent(),
                "is_retryable": err.is_retryable(),
            }),
        );
    }
    assert_golden("ts10_in_client_error_kinds", &Value::Object(got));
}

/// `timeout` + `data.retryable == false` (H1's dedicated sub-case, distinct
/// from plain `timeout`) and the four `unavailable` cause/retryable
/// combinations (only reachable through `ModelClient`).
#[tokio::test]
async fn ts10_in_model_unavailable_and_request_not_in_flight() {
    let mut got = Map::new();

    {
        let (clients, mut peer) = fake_kernel(vec!["_a24/scheduler/".to_string()]).await;
        let scheduler = SchedulerClient::new(&clients).unwrap();
        let spec = ModuleSpec::Cron {
            expr: "0 7 * * *".to_string(),
            tz: Some("UTC".to_string()),
        };
        let call =
            tokio::spawn(
                async move { scheduler.upsert("routine.x", &spec, true, None, None).await },
            );
        let req = read_request(&mut peer).await;
        respond_error_with_data(
            &mut peer,
            &req,
            -32000,
            "test",
            json!({"kind": "timeout", "retryable": false}),
        )
        .await;
        let err = call.await.unwrap().unwrap_err();
        got.insert(
            "timeout_retryable_false".to_string(),
            json!({
                "tag": client_error_tag(&err),
                "is_permanent": err.is_permanent(),
                "is_retryable": err.is_retryable(),
            }),
        );
    }

    for (cause, retryable) in [
        ("no_provider", true),
        ("request_rejected", false),
        ("backend_config", false),
        ("response_too_large", true),
    ] {
        let (clients, mut peer) = fake_kernel(vec!["_a24/model/".to_string()]).await;
        let model = ModelClient::new(&clients).unwrap();
        let req = build_classify_request(
            &fixture_task(),
            &[KeyedCandidate {
                key: "c1".to_string(),
                direction_id: "dir_focus".to_string(),
                title: "Deep Focus".to_string(),
                area_title: None,
            }],
            Engine::Local,
        );
        let call = tokio::spawn(async move { model.complete(&req).await });
        let wire_req = read_request(&mut peer).await;
        respond_error_with_data(
            &mut peer,
            &wire_req,
            -32000,
            "unavailable",
            json!({"kind": "unavailable", "cause": cause, "retryable": retryable}),
        )
        .await;
        let err = call.await.unwrap().unwrap_err();
        got.insert(
            format!("unavailable_{cause}"),
            json!({
                "tag": client_error_tag(&err),
                "is_permanent": err.is_permanent(),
                "is_retryable": err.is_retryable(),
            }),
        );
    }

    assert_golden(
        "ts10_in_unavailable_and_request_not_in_flight",
        &Value::Object(got),
    );
}

/// Usage width (§5.1): within `u32` round-trips into `ModelReply` unchanged;
/// `u32::MAX + 1` is, PRE-migration, a wire-shape parse failure
/// (`WireUsage::prompt_tokens: u32`) surfaced as `ClientError::Other` — the
/// exact behavior §5.1 says must be preserved post-migration (SDK parses as
/// `u64`, Sin90's own `u32::try_from` then fails the same way).
#[tokio::test]
async fn ts10_in_model_usage_width() {
    let mut got = Map::new();

    {
        let (clients, mut peer) = fake_kernel(vec!["_a24/model/".to_string()]).await;
        let model = ModelClient::new(&clients).unwrap();
        let req = build_classify_request(
            &fixture_task(),
            &[KeyedCandidate {
                key: "c1".to_string(),
                direction_id: "dir_focus".to_string(),
                title: "Deep Focus".to_string(),
                area_title: None,
            }],
            Engine::Local,
        );
        let call = tokio::spawn(async move { model.complete(&req).await });
        let wire_req = read_request(&mut peer).await;
        respond(
            &mut peer,
            &wire_req,
            json!({
                "text": "{}", "model_id": null, "tier": "local",
                "usage": {"prompt_tokens": 4_294_967_295u32, "completion_tokens": 1},
            }),
        )
        .await;
        let reply = call.await.unwrap().unwrap();
        got.insert(
            "within_u32".to_string(),
            json!({
                "prompt_tokens": reply.prompt_tokens,
                "completion_tokens": reply.completion_tokens,
            }),
        );
    }

    {
        let (clients, mut peer) = fake_kernel(vec!["_a24/model/".to_string()]).await;
        let model = ModelClient::new(&clients).unwrap();
        let req = build_classify_request(
            &fixture_task(),
            &[KeyedCandidate {
                key: "c1".to_string(),
                direction_id: "dir_focus".to_string(),
                title: "Deep Focus".to_string(),
                area_title: None,
            }],
            Engine::Local,
        );
        let call = tokio::spawn(async move { model.complete(&req).await });
        let wire_req = read_request(&mut peer).await;
        // 2^32 — one past `u32::MAX`, still a valid JSON number, still
        // representable losslessly in `u64`.
        respond(
            &mut peer,
            &wire_req,
            json!({
                "text": "{}", "model_id": null, "tier": "local",
                "usage": {"prompt_tokens": 4_294_967_296u64, "completion_tokens": 1},
            }),
        )
        .await;
        let err = call.await.unwrap().unwrap_err();
        got.insert(
            "overflow_u32".to_string(),
            json!({"variant": matches!(err, ClientError::Other(_))}),
        );
    }

    assert_golden("ts10_in_model_usage_width", &Value::Object(got));
}

/// Lenient parsing (§5.1): an extra, unknown field on a method's success
/// response is tolerated both before and after migration (response types
/// here carry no `deny_unknown_fields` — `clients/memory.rs`'s own module
/// doc). Two spots: a top-level extra field, and one nested inside
/// `schedule`.
#[tokio::test]
async fn ts10_in_lenient_response_parsing() {
    let (clients, mut peer) = fake_kernel(vec!["_a24/scheduler/".to_string()]).await;
    let scheduler = SchedulerClient::new(&clients).unwrap();
    let spec = ModuleSpec::Cron {
        expr: "0 7 * * *".to_string(),
        tz: Some("UTC".to_string()),
    };
    let call =
        tokio::spawn(async move { scheduler.upsert("routine.x", &spec, true, None, None).await });
    let req = read_request(&mut peer).await;
    respond(
        &mut peer,
        &req,
        json!({
            "outcome": "created",
            "future_top_level_field": "ignored",
            "schedule": {
                "key": "routine.x",
                "spec": {"type": "cron", "expr": "0 7 * * *", "tz": "UTC", "future_nested_field": 1},
                "enabled": true,
                "label": "routine.x",
                "user_suspended": false,
                "system_disabled_reason": null,
                "next_run_at": null,
                "last_fire": {"tick": null, "run_now": null},
            }
        }),
    )
    .await;
    let resp = call.await.unwrap().unwrap();
    assert_golden(
        "ts10_in_lenient_response_parsing",
        &json!({"key": resp.schedule.key, "enabled": resp.schedule.enabled}),
    );
}
