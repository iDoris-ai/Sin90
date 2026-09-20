//! Handler state + the seam that decouples `http` from Agent24 (design §5.2).

use std::sync::Arc;

use axum::http::HeaderMap;
use axum::response::Response;

use crate::http::actor::{forbidden, Actor, ActorKeys};
use crate::store::Sin90Store;

/// Where emitted events go. `adapter_agent24` implements this by forwarding to
/// `_a24/events/emit`; a `standalone`/test harness can implement it with one
/// that just records what it saw, or drops everything. Sin90's `http` layer
/// only ever sees this trait — never Agent24's actual wire format.
pub trait EventSink: Send + Sync {
    fn emit(&self, kind: &str, payload: serde_json::Map<String, serde_json::Value>);
}

/// An `EventSink` that drops every event. Used by `standalone` mode when no
/// collector is wired — the same "kernel didn't grant events → degrade, don't
/// fail" posture the ported handlers already assume (design §5.3: `events()`
/// returning `None` is fine, not an error).
pub struct NullEventSink;
impl EventSink for NullEventSink {
    fn emit(&self, _kind: &str, _payload: serde_json::Map<String, serde_json::Value>) {}
}

#[derive(Clone)]
pub struct Sin90State {
    pub store: Sin90Store,
    pub sink: Arc<dyn EventSink>,
    pub actor_keys: Arc<ActorKeys>,
}

impl Sin90State {
    pub fn new(store: Sin90Store, sink: Arc<dyn EventSink>, actor_keys: ActorKeys) -> Self {
        Self {
            store,
            sink,
            actor_keys: Arc::new(actor_keys),
        }
    }

    /// Emit `sin90.<kind>`. A non-object payload would be a bug in THIS crate
    /// (every call site passes `json!({...})`) — fails loud in debug, drops
    /// the event in release, same posture the ported kernel handlers used.
    pub fn emit(&self, kind: &str, payload: serde_json::Value) {
        let serde_json::Value::Object(map) = payload else {
            debug_assert!(false, "sin90 event payload must be an object");
            return;
        };
        self.sink.emit(kind, map);
    }

    /// Design §7.1: direct-write routes require the HUMAN actor key. The
    /// automation key identifies successfully but is still refused here — it
    /// is only good for the Proposal routes ([`Self::require_any_actor`]).
    // Err is the shared v1-envelope Response; the lint only fires because
    // Response itself is large, not because boxing it here is worthwhile.
    #[allow(clippy::result_large_err)]
    pub fn require_human(&self, headers: &HeaderMap) -> Result<(), Response> {
        match self.actor_keys.identify(headers) {
            Some(Actor::Human) => Ok(()),
            Some(Actor::Automation) => Err(forbidden(
                "the automation key may only submit/accept Proposals, not write directly",
            )),
            None => Err(forbidden("missing or unrecognized actor key")),
        }
    }

    /// The Proposal routes accept either key: a human client is free to build
    /// its own proposals too (design §7.1 does not forbid that, it only
    /// forbids AI from skipping the gate).
    #[allow(clippy::result_large_err)]
    pub fn require_any_actor(&self, headers: &HeaderMap) -> Result<(), Response> {
        match self.actor_keys.identify(headers) {
            Some(_) => Ok(()),
            None => Err(forbidden("missing or unrecognized actor key")),
        }
    }
}
