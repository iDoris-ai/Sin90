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
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::BufReader;
use tokio::net::UnixStream;
use tokio::sync::{mpsc, Mutex as AsyncMutex, Semaphore};

use crate::http::{EventSink, NullEventSink};

pub mod clients;
mod frame;
mod transport;

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

/// The capability NAMES this module tells the kernel it wants at
/// `initialize` time — a named constant, not an inline array literal in
/// [`connect_and_initialize`], specifically so
/// `tests::initialize_capabilities_matches_domain_os_yml_kernel_capabilities`
/// (L2) can assert against the exact value that gets sent, rather than a
/// hand-copied second literal that could silently drift from it. See
/// [`connect_and_initialize`]'s own comment at the call site for what the
/// kernel actually does with this field today (L1: less than it looks like).
const INITIALIZE_CAPABILITIES: &[&str] = &["events", "memory", "approval", "scheduler"];

/// How long a queued event will wait — combined, across both the sub-quota
/// gate and the main in-flight semaphore (M5) — before it is given up on and
/// dropped. Sin90's own SQLite (not the event mirror) is the source of truth
/// regardless (design §5.3); a dropped event is a degraded mirror, not data
/// loss.
const EMIT_SLOT_WAIT: Duration = Duration::from_secs(5);

/// N-M2: `KernelEventSink::emit` is a sync trait method that used to
/// `tokio::spawn` one task per call — unbounded under a heavy event burst.
/// Instead, `emit` does a non-blocking `try_send` onto a channel of this
/// capacity (full → the event is dropped and counted, not queued
/// unboundedly) drained by a FIXED pool of [`EMIT_WORKER_COUNT`] worker
/// tasks spawned once, at [`KernelEventSink::new`].
const EMIT_QUEUE_CAPACITY: usize = 256;
const EMIT_WORKER_COUNT: usize = 4;

/// N-M1: caps how many emits may be simultaneously past this gate and
/// therefore competing for `transport::Transport`'s own
/// `MAX_IN_FLIGHT_PER_CONNECTION` (64) semaphore. Bounding it below that
/// (32 < 64) guarantees at least `64 - EMIT_SUB_QUOTA` slots are always free
/// for fail-fast (non-emit) callers, no matter how many events are queued —
/// a burst of events can no longer starve, say, a scheduler call's
/// `Transport::call` of a slot just by holding onto main-semaphore permits
/// longer (via `call_with_slot_wait`) than a fail-fast caller is willing to
/// wait (which is: not at all).
const EMIT_SUB_QUOTA: usize = 32;

// Honesty check on the relationship between the two knobs above: with only
// `EMIT_WORKER_COUNT` workers, at most that many emits can ever be mid-flight
// (holding a sub-quota permit and/or a main-semaphore permit) at once — the
// worker count is what actually binds concurrency today, and the sub-quota
// is a forward-looking guard for if that count is ever raised. This assert
// is what keeps that true: it fails to COMPILE, not just a lint, if someone
// raises `EMIT_WORKER_COUNT` past `EMIT_SUB_QUOTA` without also reconsidering
// N-M1's "at least `MAX_IN_FLIGHT_PER_CONNECTION - EMIT_SUB_QUOTA` slots free
// for fail-fast callers" guarantee.
const _: () = assert!(EMIT_WORKER_COUNT <= EMIT_SUB_QUOTA);

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
/// (`docs/agent/architecture.md` §"与内核的契约"). `events` is wired up via
/// `KernelEventSink`; `scheduler`/`memory` (private)/`approval` have typed
/// clients as of T3.2.1 (`clients::Clients`, built from an `Arc<KernelClients>`
/// once the handshake is done — each one is `None` unless `Offer.provides`
/// covers its own prefix, architecture.md 不可破边界 #7). `model` is
/// deliberately NOT listed — Sin90 does not declare that capability this
/// round (L4), and the `initialize` `capabilities` field below never lists
/// it either.
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
    /// module does not guess. `pub(crate)` (L-1): nothing outside the crate
    /// needs to name `transport::TransportError`, so nothing outside needs
    /// this either.
    ///
    /// No production caller as of this commit — [`KernelEventSink`] (the
    /// only client today) uses [`Self::call_with_slot_wait`] instead. Kept,
    /// not deleted: this fail-fast form is exactly what a future typed
    /// client (T3.2.1 — scheduler/memory/approval) is expected to want
    /// ("tell the caller now, let THEM decide whether to retry" fits a
    /// business call better than emit's own bounded wait), and it is the
    /// crate's own test suite's primary way of exercising `Transport`.
    #[allow(dead_code)]
    pub(crate) async fn call(
        &self,
        method: &str,
        params: Value,
    ) -> Result<Value, transport::TransportError> {
        self.transport.call(method, params).await
    }

    /// Like [`Self::call`], but waits up to `slot_wait` for an in-flight
    /// slot instead of failing immediately — see
    /// [`transport::Transport::call_with_slot_wait`]. `pub(crate)`, same
    /// reasoning as [`Self::call`] (L-1): only [`KernelEventSink`] calls it.
    pub(crate) async fn call_with_slot_wait(
        &self,
        method: &str,
        params: Value,
        slot_wait: Duration,
    ) -> Result<Value, transport::TransportError> {
        self.transport
            .call_with_slot_wait(method, params, slot_wait)
            .await
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
            // L1 (post-review correction): the kernel's `initialize` handler
            // only DESERIALIZES this field — it does not use it to decide
            // what gets granted. Grants come only from the intersection of
            // the manifest's own `kernel_capabilities` (read once, at mount
            // time, straight from `domain-os.yml`) and the kernel's own
            // `KERNEL_OOP_GRANTS` allowlist (ME4-S1 §6.5). This list is kept
            // in sync with `domain-os.yml`'s `kernel_capabilities` by hand
            // regardless — not because the kernel reads it as authority
            // today, but so nothing here contradicts the manifest for a
            // human reader, or for a future kernel version that starts
            // consuming it (`INITIALIZE_CAPABILITIES`'s own doc; L2's test
            // pins the two staying equal). Capability NAMES here, not the
            // wire method prefixes in `SIN90_CAPABILITY_PREFIXES` — the
            // kernel's `Grants` parses these against
            // `agent24_domain::Capability::parse` (`events`/`memory`/
            // `approval`/`scheduler`, snake_case), not against a `_a24/...`
            // path.
            "capabilities": INITIALIZE_CAPABILITIES,
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
/// **No cross-event ordering guarantee.** The kernel may process concurrent
/// `_a24/events/emit` calls in any order, and [`KernelClients::call`]'s own
/// dispatch is by response id, not by call order — two events emitted in
/// quick succession can land at the event bus in either order. This is safe
/// only because Sin90's own SQLite (not the event mirror) is the source of
/// truth for everything the events describe (design §5.3, architecture.md
/// "运行形态").
///
/// `emit()` itself only ever does a non-blocking `try_send` (M5/N-M2) onto a
/// bounded channel drained by [`EMIT_WORKER_COUNT`] worker tasks spawned
/// once at [`KernelEventSink::new`] — not one `tokio::spawn` per event
/// (Codex 2026-09-22 review, Medium #6: a stalled kernel used to cost one
/// suspended task per mutation). A full queue means the event is dropped
/// (and counted via `dropped`); a worker additionally gates each dequeued
/// event through a small sub-quota semaphore (N-M1, [`EMIT_SUB_QUOTA`])
/// before it ever competes for `transport::Transport`'s own 64-slot
/// semaphore, so a burst of events can never leave fail-fast (non-emit)
/// callers permanently starved of a slot.
pub struct KernelEventSink {
    tx: mpsc::Sender<(String, Map<String, Value>)>,
    dropped: Arc<AtomicU64>,
}

impl KernelEventSink {
    pub fn new(clients: Arc<KernelClients>) -> Self {
        let (tx, rx) = mpsc::channel(EMIT_QUEUE_CAPACITY);
        let dropped = Arc::new(AtomicU64::new(0));
        let rx = Arc::new(AsyncMutex::new(rx));
        let sub_quota = Arc::new(Semaphore::new(EMIT_SUB_QUOTA));
        for _ in 0..EMIT_WORKER_COUNT {
            let clients = Arc::clone(&clients);
            let rx = Arc::clone(&rx);
            let sub_quota = Arc::clone(&sub_quota);
            tokio::spawn(async move {
                loop {
                    let item = { rx.lock().await.recv().await };
                    let Some((kind, payload)) = item else {
                        break; // every `Sender` (and `KernelEventSink`) dropped.
                    };
                    let deadline = tokio::time::Instant::now() + EMIT_SLOT_WAIT;
                    let sub_permit =
                        tokio::time::timeout_at(deadline, Arc::clone(&sub_quota).acquire_owned())
                            .await;
                    let _sub_permit = match sub_permit {
                        Ok(Ok(permit)) => permit,
                        _ => {
                            tracing::warn!(
                                kind,
                                "sin90: events/emit dropped — timed out waiting for the emit \
                                 sub-quota"
                            );
                            continue;
                        }
                    };
                    // Whatever's left of the combined budget, after the
                    // sub-quota wait, is what's left to wait for a slot on
                    // the main semaphore.
                    let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
                    let params = json!({ "kind": kind, "payload": payload });
                    if let Err(e) = clients
                        .call_with_slot_wait("_a24/events/emit", params, remaining)
                        .await
                    {
                        tracing::warn!(error = %e, kind, "sin90: events/emit failed or was dropped");
                    }
                }
            });
        }
        Self { tx, dropped }
    }

    /// How many events were dropped because the bounded queue was full
    /// (N-M2) — an observability hook, `pub(crate)` for this crate's own
    /// tests; nothing production reads it yet.
    #[cfg(test)]
    pub(crate) fn dropped_count(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

impl EventSink for KernelEventSink {
    fn emit(&self, kind: &str, payload: Map<String, Value>) {
        if self.tx.try_send((kind.to_string(), payload)).is_err() {
            let n = self.dropped.fetch_add(1, Ordering::Relaxed) + 1;
            // Log the 1st, 2nd, 4th, 8th... drop: visible, but a stalled
            // kernel can't turn this into a log flood (Codex 2026-09-22
            // review, Medium #6).
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
        Arc::new(KernelEventSink::new(clients.clone()))
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

    /// L2: pins `INITIALIZE_CAPABILITIES` against `domain-os.yml`'s
    /// `kernel_capabilities` line, so the two cannot silently drift apart —
    /// deliberately a plain string search, not a YAML parser (this crate has
    /// no YAML dependency; the kernel is the one that actually parses the
    /// manifest at mount time, per L1). Mutation: edit either list without
    /// the other and this goes red.
    #[test]
    fn initialize_capabilities_matches_domain_os_yml_kernel_capabilities() {
        let manifest = include_str!("../../domain-os.yml");
        let line = manifest
            .lines()
            .find(|l| l.trim_start().starts_with("kernel_capabilities:"))
            .expect("domain-os.yml must have a `kernel_capabilities: [...]` line");
        let open = line
            .find('[')
            .expect("kernel_capabilities must be a bracketed list");
        let close = line
            .find(']')
            .expect("kernel_capabilities list must be closed on the same line");
        let from_manifest: Vec<&str> = line[open + 1..close]
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .collect();
        assert_eq!(
            from_manifest, INITIALIZE_CAPABILITIES,
            "domain-os.yml's kernel_capabilities and INITIALIZE_CAPABILITIES (sent at \
             `initialize` time) must list the same capability names"
        );
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

    #[tokio::test]
    async fn kernel_event_sink_drops_and_counts_when_the_bounded_queue_is_full() {
        // N-M2: `emit()` itself only ever does a non-blocking `try_send` —
        // a full queue means the event is dropped, not queued unboundedly.
        // Filling it in a tight, non-yielding loop on the (default,
        // current-thread) `#[tokio::test]` runtime means none of the fixed
        // worker tasks get a chance to drain anything while we fill it, so
        // the overflow count below is exact, not a race.
        let offer = vec!["_a24/events/".to_string()];
        let (clients, _peer) = clients_over_a_socket_pair(offer);
        let sink = KernelEventSink::new(clients);

        let overflow = 44;
        for i in 0..(EMIT_QUEUE_CAPACITY + overflow) {
            sink.emit(&format!("flood.{i}"), Map::new());
        }
        assert_eq!(sink.dropped_count(), overflow as u64);
    }

    #[tokio::test]
    async fn fail_fast_calls_are_not_starved_by_a_heavy_emit_burst() {
        // N-M1: even while a heavy, sustained emit burst is in flight (each
        // one stuck holding a main-semaphore permit, since the peer never
        // reads and so no response — or write failure — ever arrives), a
        // fail-fast (non-emit) caller must still ACQUIRE A SLOT immediately.
        // With `EMIT_WORKER_COUNT` (4) far below `EMIT_SUB_QUOTA` (32) and
        // `MAX_IN_FLIGHT_PER_CONNECTION` (64), the fixed worker pool itself
        // already bounds how many slots a burst can ever occupy — this test
        // exercises that user-visible guarantee directly, whichever
        // mechanism (worker count vs. sub-quota) ends up doing the actual
        // limiting as those numbers evolve.
        let offer = vec!["_a24/events/".to_string()];
        let (clients, peer) = clients_over_a_socket_pair(offer);
        let sink = KernelEventSink::new(Arc::clone(&clients));
        let _peer = peer; // held, never read from.

        for i in 0..200 {
            sink.emit(&format!("flood.{i}"), Map::new());
        }
        // Let the fixed worker pool pick up work and start occupying
        // main-semaphore permits.
        tokio::time::sleep(Duration::from_millis(50)).await;

        let handle = tokio::spawn({
            let clients = Arc::clone(&clients);
            async move { clients.call("_a24/scheduler/upsert", json!({})).await }
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            !handle.is_finished(),
            "a fail-fast call must acquire a slot (not resolve immediately with Busy) even \
             during a heavy emit burst"
        );
        handle.abort();
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

    fn tempdir() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("sin90-test-{}", crate::core::ulid()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }
}
