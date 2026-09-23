//! The ONLY module that knows Agent24 exists (design §5.2).
//!
//! Reads `A24_DATA_DIR` / `A24_CALLBACK_SOCK` / `A24_HANDSHAKE_TOKEN` /
//! `A24_LISTEN_FD` (the real, `me3f_blackbox.rs`-verified contract, recorded
//! in `docs/STATUS.md`); runs the `initialize` handshake over the callback
//! Unix socket; accepts HTTP on the kernel-bound listener fd; wires
//! [`crate::http::EventSink`] to `_a24/events/emit` over that same socket.
//!
//! `agent24-os-sdk` (Agent24's T13) does not exist yet (design §7.2) — this is
//! the code Sin90 writes itself until it does. Only this module needs to
//! change when the SDK lands.
//!
//! # Concurrency and connection lifetime (T3.2.0)
//!
//! [`KernelClients`] holds the live connection and the `Offer` it was
//! granted, independently of any one caller — [`KernelEventSink`] is just one
//! of its users, not the owner of the channel. The connection itself is a
//! [`transport::Transport`] (single writer task, background reader task,
//! calls dispatched by id — see that module for the in-flight bound,
//! cancellation, and error semantics).
//!
//! **There is no reconnect.** The kernel serves exactly one callback
//! connection per generation (`agent24-os-proto::endpoint::CallbackListener`,
//! user decision D1): once it is gone, this generation is over. When
//! [`transport::Transport`] decides the connection is dead it runs an
//! injected hook exactly once; in production (`main.rs`) that hook is
//! `std::process::exit`, so the supervisor starts a fresh generation — with a
//! fresh process, fresh handshake, and whatever `Offer` the kernel grants
//! that time. `KernelClients`'s own `Offer` is therefore set once, at
//! construction, and never changes.

use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use std::os::unix::io::FromRawFd;
use std::sync::Arc;
use tokio::io::BufReader;
use tokio::net::UnixStream;

use crate::http::{EventSink, NullEventSink};

mod frame;
mod transport;

/// Events waiting for the worker. Past this, new events are dropped and
/// counted rather than buffered without bound.
const DEFAULT_EVENT_QUEUE: usize = 1024;

// L-1: only `FatalHook` is re-exported — `main.rs` needs to name it to
// construct `KernelClients::handshake`'s `on_fatal` argument. `TransportError`
// stays crate-internal (see `transport::TransportError`'s own doc): nothing
// outside this crate calls `KernelClients::call` (only `KernelEventSink`
// does, from inside this same module), so nothing outside needs to name the
// error type it returns.
pub use transport::FatalHook;

/// Sin90 declares it accepts `initialize` protocol versions 1 through 1000
/// (`min`/`max` below) — a wide, permissive range, NOT "exactly version 1"
/// (an earlier version of this comment said `min == max == 1`, which was
/// never true of the constants two lines down and was corrected along with
/// T3.2.0). There is only one version `me3f_blackbox.rs` exercises today;
/// the range exists so a later kernel offering, say, version 2 does not need
/// this module rebuilt just to keep negotiating successfully.
const PROTOCOL_MIN: u32 = 1;
const PROTOCOL_MAX: u32 = 1000;

#[derive(Debug, thiserror::Error)]
pub enum AdapterError {
    #[error("missing required env var {0}")]
    MissingEnv(&'static str),
    #[error("env var {0} is not a valid integer fd")]
    BadFd(&'static str),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("serialization: {0}")]
    Serde(#[from] serde_json::Error),
    #[error("handshake refused: {0}")]
    HandshakeRefused(String),
    #[error("handshake response id mismatch: sent {sent}, got {got}")]
    IdMismatch { sent: String, got: String },
    #[error(transparent)]
    Frame(#[from] frame::FrameError),
}

/// The four environment variables the kernel spawns an out-of-process module
/// with (`docs/STATUS.md`, verified against Agent24's `me3f_blackbox.rs`).
pub struct SpawnEnv {
    pub data_dir: std::path::PathBuf,
    pub callback_sock: std::path::PathBuf,
    pub handshake_token: String,
    pub listen_fd: i32,
}

impl SpawnEnv {
    pub fn from_env() -> Result<Self, AdapterError> {
        fn var(name: &'static str) -> Result<String, AdapterError> {
            std::env::var(name).map_err(|_| AdapterError::MissingEnv(name))
        }
        Ok(Self {
            data_dir: var("A24_DATA_DIR")?.into(),
            callback_sock: var("A24_CALLBACK_SOCK")?.into(),
            handshake_token: var("A24_HANDSHAKE_TOKEN")?,
            listen_fd: var("A24_LISTEN_FD")?
                .parse()
                .map_err(|_| AdapterError::BadFd("A24_LISTEN_FD"))?,
        })
    }
}

/// sha256 digest of the manifest, formatted `sha256:<hex>` — the exact shape
/// `initialize`'s `manifest_digest` field expects (`docs/STATUS.md`).
pub fn manifest_digest(manifest_bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(manifest_bytes);
    format!("sha256:{}", hex::encode(hasher.finalize()))
}

/// Capability prefixes Sin90 has any use for, per the kernel contract table
/// (`docs/agent/architecture.md` §"与内核的契约"). Only `events` is wired up
/// today (`KernelEventSink`); `scheduler`/`memory` (private)/`approval` get
/// typed clients in T3.2.1. `model` is deliberately NOT listed — Sin90 does
/// not declare that capability this round (L4), and the `initialize`
/// `capabilities` field below stays `["events"]` only; T3.2.1 is what
/// expands both together.
pub const SIN90_CAPABILITY_PREFIXES: &[&str] = &[
    "_a24/events/",
    "_a24/scheduler/",
    "_a24/memory/private/",
    "_a24/approval/",
];

/// Whether `offer` grants at least one prefix from
/// [`SIN90_CAPABILITY_PREFIXES`] — the decoupled "keep the callback channel
/// open" decision (design §5), separate from "is `events` specifically among
/// them" (only [`wire_kernel_clients`]'s `EventSink` choice cares about
/// that).
pub fn provides_any_known_capability(offer: &[String]) -> bool {
    offer.iter().any(|granted| {
        SIN90_CAPABILITY_PREFIXES
            .iter()
            .any(|known| granted.starts_with(known) || known.starts_with(granted.as_str()))
    })
}

/// Independently holds the callback channel (a [`transport::Transport`]) and
/// the `Offer` it was granted at handshake time — [`KernelEventSink`] is
/// just one of this struct's users. No reconnect, no mutable `Offer` after
/// construction — see module docs for why.
pub struct KernelClients {
    transport: transport::Transport,
    offer: Vec<String>,
}

impl KernelClients {
    /// Connect and run `initialize`. `on_fatal` runs exactly once, the
    /// moment the underlying [`transport::Transport`] decides the connection
    /// is dead (production: exit the process; tests: record that it
    /// happened) — see [`transport`]'s module docs.
    pub async fn handshake(
        sock_path: &std::path::Path,
        module: &str,
        manifest_bytes: &[u8],
        auth_token: &str,
        on_fatal: FatalHook,
    ) -> Result<(Self, Vec<String>), AdapterError> {
        let (stream, offer) =
            connect_and_initialize(sock_path, module, manifest_bytes, auth_token).await?;
        let transport = transport::Transport::spawn(stream, on_fatal);
        let clients = Self {
            transport,
            offer: offer.clone(),
        };
        Ok((clients, offer))
    }

    /// The `Offer.provides` this connection was granted at handshake time —
    /// fixed for the connection's whole life (no reconnect to update it
    /// from).
    pub fn offer(&self) -> &[String] {
        &self.offer
    }

    /// Mirrors `agent24-os-proto::initialize::Offer::provides`: unbounded
    /// prefix match of a concrete method name against the granted `Offer`.
    pub fn provides(&self, method: &str) -> bool {
        self.offer.iter().any(|p| method.starts_with(p.as_str()))
    }

    /// Whether the underlying connection is still up. Once this turns
    /// `false` it never turns back `true` — see module docs (no reconnect).
    pub fn is_alive(&self) -> bool {
        self.transport.is_alive()
    }

    /// Call a method through the transport. Fails fast (no waiting for a
    /// slot) when 64 calls are already in flight — see
    /// [`transport::Transport::call`]. Never retries: a
    /// `ConnectionLost` means the outcome is genuinely unknown, and this
    /// module does not guess. `pub(crate)` (L-1): only [`KernelEventSink`],
    /// inside this crate, calls it today — nothing outside the crate needs
    /// to name `transport::TransportError`, so nothing outside needs this
    /// either. A future typed client (T3.2.1) that also lives inside
    /// `adapter_agent24` can reach it the same way `KernelEventSink` does.
    pub(crate) async fn call(
        &self,
        method: &str,
        params: Value,
    ) -> Result<Value, transport::TransportError> {
        self.transport.call(method, params).await
    }

    /// Builds a [`KernelClients`] over an already-spawned
    /// [`transport::Transport`], skipping the dial-and-`initialize` step —
    /// used by [`wire_kernel_clients`]'s own tests to inject an arbitrary
    /// `Offer` without a real kernel on the other end.
    #[cfg(test)]
    fn from_transport(transport: transport::Transport, offer: Vec<String>) -> Self {
        Self { transport, offer }
    }
}

/// Connect to `sock_path` and run the `initialize` exchange — the one place
/// that speaks the wire format for [`KernelClients::handshake`].
async fn connect_and_initialize(
    sock_path: &std::path::Path,
    module: &str,
    manifest_bytes: &[u8],
    auth_token: &str,
) -> Result<(UnixStream, Vec<String>), AdapterError> {
    let stream = UnixStream::connect(sock_path).await?;
    let mut reader = BufReader::new(stream);

    let id = "1".to_string();
    let req = json!({
        "jsonrpc": "2.0",
        "method": "initialize",
        "id": id,
        "params": {
            "protocol_versions": { "min": PROTOCOL_MIN, "max": PROTOCOL_MAX },
            "module": module,
            "manifest_digest": manifest_digest(manifest_bytes),
            "auth_token": auth_token,
            "capabilities": ["events"],
        }
    });
    frame::write_frame(reader.get_mut(), &req).await?;

    let line = frame::read_frame(&mut reader).await?;
    let resp: Value = serde_json::from_slice(&line)?;
    let got_id = resp["id"].as_str().unwrap_or_default().to_string();
    if got_id != id {
        return Err(AdapterError::IdMismatch {
            sent: id,
            got: got_id,
        });
    }
    if let Some(err) = resp.get("error") {
        return Err(AdapterError::HandshakeRefused(err.to_string()));
    }
    let provides: Vec<String> = resp["result"]["offer"]["provides"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();

    // Nothing should have arrived on the socket beyond this one handshake
    // response — the kernel has nothing to say until we send it another
    // request, and we have not yet. If the `BufReader` nonetheless has
    // unconsumed bytes buffered, handing back only `reader.into_inner()`
    // would silently drop them, so that case is treated as a protocol
    // violation rather than risking lost bytes on the connection
    // `transport::Transport` is about to take over.
    if !reader.buffer().is_empty() {
        return Err(AdapterError::HandshakeRefused(
            "kernel sent unexpected extra bytes immediately after the initialize response"
                .to_string(),
        ));
    }
    Ok((reader.into_inner(), provides))
}

/// Adapts [`KernelClients`] to [`crate::http::EventSink`]. `http` never sees
/// this type — only the trait.
///
/// One worker drains a bounded queue, so events reach the kernel in the order
/// they were emitted and a stalled kernel costs at most the queue, not one
/// suspended task per mutation (Codex 2026-09-22 review, Medium #6). When the
/// queue is full the event is dropped and counted — events are best-effort by
/// design (§5.3), a committed write must not wait on them.
///
/// Emitting through the worker (rather than a bare `tokio::spawn` per event)
/// also means at most one `_a24/events/emit` call is in flight at a time from
/// this sink, so two events queued in quick succession cannot race each other
/// on the wire — though this is still safe either way only because Sin90's
/// own SQLite (not the event mirror) is the source of truth for everything
/// the events describe (design §5.3, architecture.md "运行形态").
pub struct KernelEventSink {
    tx: tokio::sync::mpsc::Sender<(String, Map<String, Value>)>,
    dropped: Arc<std::sync::atomic::AtomicU64>,
}

impl KernelEventSink {
    /// Must be called inside a Tokio runtime (spawns the worker).
    pub fn spawn(clients: Arc<KernelClients>) -> Self {
        Self::with_capacity(clients, DEFAULT_EVENT_QUEUE)
    }

    pub fn with_capacity(clients: Arc<KernelClients>, capacity: usize) -> Self {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<(String, Map<String, Value>)>(capacity);
        tokio::spawn(async move {
            while let Some((kind, payload)) = rx.recv().await {
                let params = json!({ "kind": kind, "payload": payload });
                if let Err(e) = clients.call("_a24/events/emit", params).await {
                    tracing::warn!(error = %e, kind, "sin90: events/emit failed");
                }
            }
        });
        Self {
            tx,
            dropped: Arc::new(std::sync::atomic::AtomicU64::new(0)),
        }
    }

    /// Events dropped so far because the queue was full (or the worker gone).
    pub fn dropped(&self) -> u64 {
        self.dropped.load(std::sync::atomic::Ordering::Relaxed)
    }
}

impl EventSink for KernelEventSink {
    fn emit(&self, kind: &str, payload: Map<String, Value>) {
        if self.tx.try_send((kind.to_string(), payload)).is_err() {
            let n = self
                .dropped
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                + 1;
            // Log the 1st, 2nd, 4th, 8th... drop: visible, but a stalled
            // kernel can't turn this into a log flood.
            if n.is_power_of_two() {
                tracing::warn!(
                    kind,
                    dropped_total = n,
                    "sin90: event queue full, dropping event"
                );
            }
        }
    }
}

/// The decision `main.rs`'s `run_as_agent24_module` actually applies —
/// pulled out here so it can be exercised with an INJECTED `Offer` and a
/// [`KernelClients`] built over an in-memory socket pair, no real kernel or
/// handshake required. `main.rs` calls this verbatim; it does not
/// reimplement the decision, so a bug in how the decision gets APPLIED (not
/// just in the decision itself) shows up here too.
///
/// # N-H1: the callback connection is ALWAYS returned, never optional
///
/// An earlier version of this function returned `Option<Arc<KernelClients>>`
/// — `None` when the kernel offered nothing Sin90 uses. That was a bug, not
/// a feature: dropping the last `Arc<KernelClients>` drops its
/// `transport::Transport`, which closes the socket — and the kernel serves
/// exactly one callback connection per generation (design §5, D1), so
/// closing it is indistinguishable from this generation crashing. The
/// process then exits via `on_fatal` (as it should for a truly dead
/// connection), the supervisor restarts a fresh generation, THAT generation
/// negotiates the same empty `Offer` again (nothing about the manifest
/// changed), and the cycle repeats — a restart storm bounded only by the
/// supervisor's circuit breaker, for a module that was doing nothing wrong.
///
/// The fix: the connection is the generation's lifeline regardless of what
/// business capability, if any, got granted. `Offer` only decides which
/// CLIENT gets wired to it (today: `EventSink`, decoupled from "is `events`
/// specifically granted" so a future scheduler/memory/approval client,
/// T3.2.1, is not penalized for events being absent) — never whether the
/// connection itself survives. The caller (`main.rs`) must hold the returned
/// `Arc<KernelClients>` for the rest of the process's life either way.
pub fn wire_kernel_clients(
    offer: &[String],
    clients: Arc<KernelClients>,
) -> (Arc<dyn EventSink>, Arc<KernelClients>) {
    if !provides_any_known_capability(offer) {
        tracing::warn!(
            "sin90: kernel offered no capability Sin90 uses; the callback connection is kept \
             open regardless (N-H1 — closing it would end this generation) but no client is \
             wired to it"
        );
        return (Arc::new(NullEventSink), clients);
    }
    let sink: Arc<dyn EventSink> = if clients.provides("_a24/events/emit") {
        Arc::new(KernelEventSink::spawn(clients.clone()))
    } else {
        // Granted some other capability but not events — degrade, don't
        // fail (design §5.3).
        tracing::warn!("sin90: events not offered by kernel; running with events dropped");
        Arc::new(NullEventSink)
    };
    (sink, clients)
}

/// Build a `tokio::net::UnixListener` from the kernel-bound `A24_LISTEN_FD`.
///
/// Agent24's `agent24-os-proto::launch::LaunchSpec::listener` is a
/// `std::os::unix::net::UnixListener` (FU-60: "a Unix domain socket, not a
/// TCP port") — wrapping this fd as a `TcpListener` compiles and even binds,
/// but every real `accept()` on it then fails with `EINVAL` ("invalid input
/// parameter"), because the fd's actual address family is `AF_UNIX`, not
/// `AF_INET`. Caught by `tests/agent24_mount_blackbox.rs` against a real
/// daemon — no unit test using a loopback TCP or Unix socket of its own
/// choosing could have caught this, since both sides would agree with
/// themselves about which family to use.
///
/// # Safety
/// The fd is a real, kernel-opened, non-owned-by-anything-else listener
/// socket for the lifetime of this process — that is the entire contract
/// `A24_LISTEN_FD` exists to state (`docs/STATUS.md`). This function must be
/// called at most once per process.
pub fn listener_from_fd(fd: i32) -> std::io::Result<tokio::net::UnixListener> {
    let std_listener = unsafe { std::os::unix::net::UnixListener::from_raw_fd(fd) };
    std_listener.set_nonblocking(true)?;
    tokio::net::UnixListener::from_std(std_listener)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};

    /// A `KernelClients` over one half of an in-memory socket pair (the
    /// other half stays a real, async, readable `UnixStream` a test can
    /// probe for EOF/data) plus a `Transport` behind it, no real kernel or
    /// handshake required. `offer` is injected directly.
    fn clients_over_a_socket_pair(offer: Vec<String>) -> (Arc<KernelClients>, UnixStream) {
        let (a, b) = std::os::unix::net::UnixStream::pair().unwrap();
        a.set_nonblocking(true).unwrap();
        b.set_nonblocking(true).unwrap();
        let stream = UnixStream::from_std(a).unwrap();
        let peer = UnixStream::from_std(b).unwrap();
        let transport = transport::Transport::spawn(stream, noop_hook());
        (
            Arc::new(KernelClients::from_transport(transport, offer)),
            peer,
        )
    }

    fn noop_hook() -> FatalHook {
        Arc::new(|| {})
    }

    #[test]
    fn manifest_digest_is_sha256_prefixed_hex() {
        let d = manifest_digest(b"hello");
        assert!(d.starts_with("sha256:"));
        assert_eq!(d.len(), "sha256:".len() + 64);
        // Stable, known vector.
        assert_eq!(
            d,
            "sha256:2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );
    }

    #[tokio::test]
    async fn handshake_over_a_loopback_unix_socket_round_trips() {
        let dir = tempdir();
        let sock_path = dir.join("cb.sock");
        let listener = tokio::net::UnixListener::bind(&sock_path).unwrap();

        let manifest = b"name: sin90\n";
        let token = "test-token";
        let server = tokio::spawn({
            let manifest = manifest.to_vec();
            let token = token.to_string();
            async move {
                let (stream, _) = listener.accept().await.unwrap();
                let mut reader = BufReader::new(stream);
                let mut buf = Vec::new();
                reader.read_until(b'\n', &mut buf).await.unwrap();
                let req: Value = serde_json::from_slice(&buf).unwrap();
                assert_eq!(req["method"], "initialize");
                assert_eq!(req["params"]["auth_token"], token);
                assert_eq!(req["params"]["manifest_digest"], manifest_digest(&manifest));
                let resp = json!({
                    "jsonrpc": "2.0", "id": req["id"],
                    "result": { "protocol_version": 1, "offer": { "provides": ["_a24/events/"] } }
                });
                let mut bytes = serde_json::to_vec(&resp).unwrap();
                bytes.push(b'\n');
                reader.get_mut().write_all(&bytes).await.unwrap();
            }
        });

        let (clients, offer) =
            KernelClients::handshake(&sock_path, "sin90", manifest, token, noop_hook())
                .await
                .unwrap();
        assert_eq!(offer, vec!["_a24/events/".to_string()]);
        assert_eq!(clients.offer(), offer.as_slice());
        assert!(clients.provides("_a24/events/emit"));
        assert!(!clients.provides("_a24/scheduler/upsert"));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn handshake_refusal_surfaces_as_an_error() {
        let dir = tempdir();
        let sock_path = dir.join("cb.sock");
        let listener = tokio::net::UnixListener::bind(&sock_path).unwrap();

        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut reader = BufReader::new(stream);
            let mut buf = Vec::new();
            reader.read_until(b'\n', &mut buf).await.unwrap();
            let req: Value = serde_json::from_slice(&buf).unwrap();
            let resp = json!({
                "jsonrpc": "2.0", "id": req["id"],
                "error": { "code": -32000, "kind": "auth_failed", "message": "bad token" }
            });
            let mut bytes = serde_json::to_vec(&resp).unwrap();
            bytes.push(b'\n');
            reader.get_mut().write_all(&bytes).await.unwrap();
        });

        let result =
            KernelClients::handshake(&sock_path, "sin90", b"x", "wrong-token", noop_hook()).await;
        let Err(err) = result else {
            panic!("expected the handshake to be refused");
        };
        assert!(matches!(err, AdapterError::HandshakeRefused(_)));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn kernel_clients_call_round_trips_through_the_real_handshake() {
        let dir = tempdir();
        let sock_path = dir.join("cb.sock");
        let listener = tokio::net::UnixListener::bind(&sock_path).unwrap();
        let manifest = b"name: sin90\n";
        let token = "test-token";

        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut reader = BufReader::new(stream);
            let mut buf = Vec::new();
            reader.read_until(b'\n', &mut buf).await.unwrap();
            let req: Value = serde_json::from_slice(&buf).unwrap();
            let resp = json!({
                "jsonrpc": "2.0", "id": req["id"],
                "result": { "protocol_version": 1, "offer": { "provides": ["_a24/events/"] } }
            });
            let mut bytes = serde_json::to_vec(&resp).unwrap();
            bytes.push(b'\n');
            reader.get_mut().write_all(&bytes).await.unwrap();

            buf.clear();
            reader.read_until(b'\n', &mut buf).await.unwrap();
            let call: Value = serde_json::from_slice(&buf).unwrap();
            assert_eq!(call["method"], "_a24/events/emit");
            let resp = json!({"jsonrpc": "2.0", "id": call["id"], "result": {}});
            let mut bytes = serde_json::to_vec(&resp).unwrap();
            bytes.push(b'\n');
            reader.get_mut().write_all(&bytes).await.unwrap();
        });

        let (clients, _offer) =
            KernelClients::handshake(&sock_path, "sin90", manifest, token, noop_hook())
                .await
                .unwrap();
        let result = clients
            .call("_a24/events/emit", json!({"kind": "test", "payload": {}}))
            .await
            .unwrap();
        assert_eq!(result, json!({}));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn wiring_keeps_the_callback_connection_open_even_when_nothing_is_offered() {
        // N-H1: the whole point of the fix — the connection must survive
        // regardless of what `wire_kernel_clients` decided about wiring a
        // client, because dropping it is how the kernel decides this
        // generation is over.
        let offer: Vec<String> = vec![];
        let (clients, mut peer) = clients_over_a_socket_pair(offer.clone());

        let (_sink, holder) = wire_kernel_clients(&offer, clients);

        let mut buf = [0u8; 1];
        let read = tokio::time::timeout(Duration::from_millis(200), peer.read(&mut buf)).await;
        assert!(
            read.is_err(),
            "the peer must not see EOF while the returned KernelClients handle is held, even \
             though nothing was offered"
        );
        drop(holder); // keep it alive up to here, not a moment less.
    }

    #[tokio::test]
    async fn wiring_positive_control_dropping_the_returned_holder_does_close_the_connection() {
        // Positive control for the test above, and a demonstration of
        // exactly what the N-H1 bug looked like: drop the handle
        // `wire_kernel_clients` handed back (what the old `None` branch
        // effectively did), and the peer genuinely does see EOF. This is
        // why `main.rs` binding `_clients` (not `_`) to the returned value
        // for the rest of its function is load-bearing, not decorative.
        let offer: Vec<String> = vec![];
        let (clients, mut peer) = clients_over_a_socket_pair(offer.clone());

        let (_sink, holder) = wire_kernel_clients(&offer, clients);
        drop(holder);

        let mut buf = [0u8; 1];
        let read = tokio::time::timeout(Duration::from_millis(200), peer.read(&mut buf)).await;
        match read {
            Ok(Ok(0)) => {} // EOF, as expected.
            other => panic!("expected EOF after dropping the holder, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn wiring_uses_null_event_sink_when_only_scheduler_offered_not_events() {
        // N-M4③: assert the CONCRETE sink behavior, not an `Option`'s
        // shape — a `NullEventSink` must never put anything on the wire.
        let offer = vec!["_a24/scheduler/".to_string()];
        let (clients, mut peer) = clients_over_a_socket_pair(offer.clone());

        let (sink, _holder) = wire_kernel_clients(&offer, clients);
        sink.emit("test.kind", Map::new());

        let mut buf = [0u8; 1];
        let read = tokio::time::timeout(Duration::from_millis(200), peer.read(&mut buf)).await;
        assert!(
            read.is_err(),
            "NullEventSink must never write anything to the wire"
        );
    }

    #[tokio::test]
    async fn wiring_uses_kernel_event_sink_and_emit_reaches_the_wire_when_events_offered() {
        // N-M4③ positive control: `events` granted → emit really goes out.
        let offer = vec!["_a24/events/".to_string()];
        let (clients, peer) = clients_over_a_socket_pair(offer.clone());
        let mut peer_reader = BufReader::new(peer);

        let (sink, _holder) = wire_kernel_clients(&offer, clients);
        sink.emit("test.kind", Map::new());

        let mut buf = Vec::new();
        tokio::time::timeout(
            Duration::from_secs(1),
            peer_reader.read_until(b'\n', &mut buf),
        )
        .await
        .expect("KernelEventSink must actually write to the wire")
        .unwrap();
        let parsed: Value = serde_json::from_slice(&buf).unwrap();
        assert_eq!(parsed["method"], "_a24/events/emit");
    }

    #[test]
    fn on_fatal_is_a_plain_send_sync_closure_main_rs_can_construct_without_naming_transport() {
        // main.rs (a different crate) must be able to build a `FatalHook`
        // without ever naming `adapter_agent24::transport` (private module,
        // L5) — only the re-exported alias.
        let count = Arc::new(AtomicUsize::new(0));
        let hook: FatalHook = {
            let count = count.clone();
            Arc::new(move || {
                count.fetch_add(1, Ordering::SeqCst);
            })
        };
        hook();
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }

    /// Medium #6: every mutation used to spawn its own task, each waiting
    /// behind the stuck one. Now one worker drains a bounded queue: overflow
    /// is dropped and counted rather than piling up one suspended task per
    /// event. Uses the in-memory socket pair (no real kernel needed) with a
    /// peer that reads but never answers, so the worker is provably stuck
    /// inside its first round trip while the rest of the queue fills up.
    #[tokio::test]
    async fn event_queue_is_bounded_and_counts_drops_while_the_kernel_is_stuck() {
        let offer = vec!["_a24/events/".to_string()];
        let (clients, peer) = clients_over_a_socket_pair(offer);
        let mut peer_reader = BufReader::new(peer);

        let sink = KernelEventSink::with_capacity(clients, 4);

        // First event: wait until the peer actually has the request, so the
        // worker is known to be parked inside that (never-answered) round
        // trip before the rest of the queue fills up.
        sink.emit("e0", Map::new());
        let mut buf = Vec::new();
        tokio::time::timeout(
            Duration::from_secs(1),
            peer_reader.read_until(b'\n', &mut buf),
        )
        .await
        .expect("the worker must have sent e0's request")
        .unwrap();

        for i in 1..=14 {
            sink.emit(&format!("e{i}"), Map::new());
        }
        assert_eq!(sink.dropped(), 10, "4 queued, the other 10 dropped");

        // Positive control: nothing else reached the peer while it's stuck.
        let mut extra = [0u8; 1];
        let read = tokio::time::timeout(
            Duration::from_millis(200),
            peer_reader.get_mut().read(&mut extra),
        )
        .await;
        assert!(
            read.is_err(),
            "no further events should reach the wire while the worker is stuck"
        );
    }

    fn tempdir() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("sin90-test-{}", crate::core::ulid()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }
}
