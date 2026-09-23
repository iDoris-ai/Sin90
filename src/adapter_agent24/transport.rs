//! T3.2.0 — the multiplexed callback transport (design §5: "回调通道多路复用").
//!
//! One physical Unix socket, split once (`UnixStream::into_split`): a single
//! **writer task** owns the write half — nothing else ever writes, so two
//! concurrent calls' frames can never interleave mid-write on the wire — and
//! a background **reader task** owns the read half, dispatching every
//! response by its `id` to whichever [`Transport::call`] is still waiting on
//! it (`pending`). The kernel owes no ordering guarantee on responses
//! (SPEC), so a response is matched by id, never by call order.
//!
//! # No reconnect — one `Transport` is one connection's whole life
//!
//! The kernel serves exactly one callback connection per generation: the
//! listener is closed and its path removed the instant the first connection
//! is accepted (`agent24-os-proto::endpoint::CallbackListener::accept_one`,
//! user decision D1 — a generation that loses its connection does not get a
//! second one). A `Transport` therefore never tries to reconnect; when it
//! decides the connection is dead it tears itself down and calls the
//! injected [`FatalHook`] exactly once — what that hook DOES (exit the
//! process so the supervisor restarts a fresh generation, in production; bump
//! a counter, in a test) is entirely up to whoever constructed it. That
//! wiring lives one layer up, in `adapter_agent24::KernelClients`.
//!
//! # In-flight bound
//!
//! [`MAX_IN_FLIGHT_PER_CONNECTION`] (64) happens to equal Agent24's own
//! `agent24_os_proto::rpc::MAX_IN_FLIGHT_PER_CONNECTION` — a convenient
//! coincidence to reason about, not an enforced symmetry (nothing ties the
//! two numbers together; if the kernel's changes, this one does not follow
//! automatically). `tokio::sync::Semaphore::try_acquire_owned` bounds it: the
//! 65th concurrent call is answered [`TransportError::Busy`] immediately,
//! not queued — a queue would be memory sized by however many calls the
//! caller's own code happens to issue at once, the same reasoning behind the
//! kernel rejecting a queue on ITS side of the same-shaped limit.
//!
//! # Errors: what a caller can safely do next
//!
//! [`TransportError`] splits "did the kernel possibly see this" into two
//! cases, because only one of them is safe to retry:
//! - [`TransportError::NotSent`]: the call never reached the wire — the
//!   connection was already known dead when `call()` tried to send it. A
//!   caller may retry with a NEW connection (there is not going to be one on
//!   THIS `Transport` — see above).
//! - [`TransportError::ConnectionLost`]: the call was in flight (already
//!   written, or handed to the writer) when the connection died. Whether the
//!   kernel received and/or acted on it is **unknown** — retrying blindly
//!   could double a side effect (`_a24/events/emit` today; scheduler upserts
//!   later). This module never retries either case itself.
//!
//! # Cancellation
//!
//! [`Transport::call`]'s returned future holds a [`CallGuard`] across its
//! `.await`. Dropping that future — explicit drop, a lost `tokio::select!`
//! race, or [`TransportError::Timeout`] firing — runs [`CallGuard::drop`],
//! which frees the semaphore permit and, only if the call had not yet been
//! answered, best-effort-queues (`try_send`, never blocks a `Drop`) the
//! wire-format `$/cancelRequest` **notification**
//! (`agent24-os-proto::rpc::CANCEL_METHOD`, `rpc.rs:94-95`): no top-level
//! `id`, `params: {"id": <the id being cancelled>}`. A late response after
//! the slot has been freed and reused finds no entry in `pending` and is
//! dropped — never a panic.

use std::collections::hash_map::Entry;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use serde_json::{json, Value};
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::{mpsc, oneshot, Notify, Semaphore};
use tokio::task::JoinHandle;

use crate::adapter_agent24::frame;

/// Mirrors `agent24-os-proto::rpc::MAX_IN_FLIGHT_PER_CONNECTION` (`rpc.rs`
/// ~L64) in VALUE only — see module docs.
pub(crate) const MAX_IN_FLIGHT_PER_CONNECTION: usize = 64;

/// Mirrors `agent24-os-proto::rpc::CANCEL_METHOD` (`rpc.rs:94-95`).
pub(crate) const CANCEL_METHOD: &str = "$/cancelRequest";

/// How long a write (including the mandatory flush) may take before the
/// connection is considered dead. A module that stops being written to must
/// not be able to stall this side of the connection forever.
pub(crate) const WRITE_TIMEOUT: Duration = Duration::from_secs(10);

/// Fallback deadline for one call's response, strictly greater than the
/// kernel's own `agent24_os_proto::rpc::CALL_TIMEOUT` (30s) so that under
/// normal operation the KERNEL's timeout fires first and this is only a
/// backstop against a response that never arrives at all (a kernel bug, or a
/// connection that is silently wedged rather than cleanly dead).
pub(crate) const RESPONSE_TIMEOUT: Duration = Duration::from_secs(35);

/// Writer queue depth: enough for every in-flight call's request (bounded by
/// the semaphore at [`MAX_IN_FLIGHT_PER_CONNECTION`]) plus headroom for
/// `$/cancelRequest` notifications racing in around the same time.
const WRITER_QUEUE_CAPACITY: usize = 2 * MAX_IN_FLIGHT_PER_CONNECTION + 8;

/// A hook run exactly once, the moment a [`Transport`] decides its
/// connection is dead. Production code (`main.rs`) makes this exit the
/// process; tests make it record that it ran.
/// `pub`, not `pub(crate)` (L-1): `KernelClients::handshake`'s public
/// signature takes one, and `main.rs` (a different crate from this lib)
/// needs to be able to construct that argument's type — `TransportError` and
/// `RpcErrorInfo` stay `pub(crate)` since nothing outside this crate needs to
/// name them (`KernelClients::call` is `pub(crate)` too; only
/// `KernelEventSink`, inside this crate, calls it).
pub type FatalHook = Arc<dyn Fn() + Send + Sync>;

/// The kernel's `error.data.kind` and the rest of an RPC error, parsed once
/// here rather than left as a `Value` every caller re-navigates. `T3.2.1`'s
/// typed clients match on `kind`; `raw` keeps the original payload for
/// anything not typed yet — genuinely unread by anything in this crate
/// today (only `code`/`kind`/`message` are, via `Display`), which is why it
/// alone still carries `#[allow(dead_code)]`.
#[derive(Debug, Clone)]
pub(crate) struct RpcErrorInfo {
    pub code: i64,
    pub kind: Option<String>,
    pub message: String,
    #[allow(dead_code)]
    pub raw: Value,
}

impl RpcErrorInfo {
    fn from_json(err: &Value) -> Self {
        Self {
            code: err.get("code").and_then(Value::as_i64).unwrap_or(0),
            kind: err
                .get("data")
                .and_then(|d| d.get("kind"))
                .and_then(Value::as_str)
                .map(str::to_string),
            message: err
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            raw: err.clone(),
        }
    }
}

impl std::fmt::Display for RpcErrorInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.kind {
            Some(kind) => write!(f, "{kind} ({}): {}", self.code, self.message),
            None => write!(f, "({}): {}", self.code, self.message),
        }
    }
}

#[derive(Debug, Clone, thiserror::Error)]
pub(crate) enum TransportError {
    /// The call never reached the wire. Safe to retry — on a new connection;
    /// see module docs.
    #[error("callback connection is closed; the call was never sent")]
    NotSent,
    /// The call was in flight when the connection died. Its outcome is
    /// UNKNOWN. Never retried automatically by this module.
    #[error("callback connection was lost while this call was in flight; its outcome is unknown")]
    ConnectionLost,
    #[error("{MAX_IN_FLIGHT_PER_CONNECTION} calls already in flight on this connection")]
    Busy,
    #[error("params would serialize to more than {} bytes", frame::MAX_FRAME_BYTES)]
    FrameTooLarge,
    #[error("kernel rejected the call: {0}")]
    Rpc(RpcErrorInfo),
    #[error("internal bug: call id already in flight on this connection")]
    IdCollision,
    /// No response within [`RESPONSE_TIMEOUT`]. Treated the same as a
    /// caller-side cancel: `$/cancelRequest` is sent and the slot freed.
    #[error("no response within the fallback deadline")]
    Timeout,
}

struct PendingState {
    /// Set exactly once, by whichever of the writer/reader tasks first
    /// decides the connection is dead — see [`declare_dead`]. Checked inside
    /// this same lock at `call()`'s insert step so "the connection is
    /// already closed" and "insert my waiter" can never race each other.
    closed: bool,
    map: HashMap<String, oneshot::Sender<Result<Value, TransportError>>>,
}

type Pending = Arc<StdMutex<PendingState>>;

/// Wakes the writer/reader tasks when the connection dies, and makes sure
/// [`FatalHook`] runs exactly once even if both tasks notice at once.
struct DeathSignal {
    fired: AtomicBool,
    notify: Notify,
}

/// Runs exactly once per connection, the first time either task calls it:
/// fails every still-pending call with [`TransportError::ConnectionLost`],
/// closes `semaphore` (N-M3: any `acquire_owned`/`call_with_slot_wait`
/// already waiting for a slot wakes immediately with an error instead of
/// riding out its whole timeout), wakes the other task (which is either
/// already watching [`DeathSignal::notify`] via the enable-before-check
/// pattern in the reader/writer loops below, or will see `fired` at its next
/// loop-top check), and finally runs [`FatalHook`].
fn declare_dead(
    death: &DeathSignal,
    pending: &Pending,
    semaphore: &Semaphore,
    on_fatal: &FatalHook,
) {
    if death.fired.swap(true, Ordering::SeqCst) {
        return; // the other side already ran this.
    }
    fail_all_pending(pending, TransportError::ConnectionLost);
    semaphore.close();
    death.notify.notify_waiters();
    on_fatal();
}

/// Configurable so tests can use a millisecond-scale write deadline instead
/// of actually waiting out [`WRITE_TIMEOUT`]. The response deadline is a
/// separate, per-call override ([`Transport::call_with_timeout`]) rather
/// than living here, since `call()` needs to pick it per call, not once per
/// connection. Production code (`KernelClients`) always uses
/// [`TransportConfig::default`].
#[derive(Debug, Clone, Copy)]
pub(crate) struct TransportConfig {
    pub write_timeout: Duration,
}

impl Default for TransportConfig {
    fn default() -> Self {
        Self {
            write_timeout: WRITE_TIMEOUT,
        }
    }
}

/// A live, multiplexed connection to the kernel's callback socket, past the
/// `initialize` handshake. Built from an already-connected [`UnixStream`] by
/// [`Transport::spawn`] — this module does not know how to dial or
/// handshake; that stays in `adapter_agent24::mod` (design §8: adapter owns
/// the wire protocol, this module owns concurrency over an established
/// connection). Owned by `adapter_agent24::KernelClients`, one per
/// connection, for that connection's whole life — no reconnect (module
/// docs).
pub(crate) struct Transport {
    write_tx: mpsc::Sender<Vec<u8>>,
    pending: Pending,
    next_id: AtomicU64,
    semaphore: Arc<Semaphore>,
    death: Arc<DeathSignal>,
    writer_task: JoinHandle<()>,
    reader_task: JoinHandle<()>,
}

impl Transport {
    pub(crate) fn spawn(stream: UnixStream, on_fatal: FatalHook) -> Self {
        Self::spawn_with_config(stream, on_fatal, TransportConfig::default())
    }

    pub(crate) fn spawn_with_config(
        stream: UnixStream,
        on_fatal: FatalHook,
        config: TransportConfig,
    ) -> Self {
        let (read_half, write_half) = stream.into_split();
        let (write_tx, mut write_rx) = mpsc::channel::<Vec<u8>>(WRITER_QUEUE_CAPACITY);
        let pending: Pending = Arc::new(StdMutex::new(PendingState {
            closed: false,
            map: HashMap::new(),
        }));
        let death = Arc::new(DeathSignal {
            fired: AtomicBool::new(false),
            notify: Notify::new(),
        });
        let semaphore = Arc::new(Semaphore::new(MAX_IN_FLIGHT_PER_CONNECTION));

        let writer_death = Arc::clone(&death);
        let writer_pending = Arc::clone(&pending);
        let writer_semaphore = Arc::clone(&semaphore);
        let writer_on_fatal = Arc::clone(&on_fatal);
        let write_timeout = config.write_timeout;
        let writer_task = tokio::spawn(async move {
            let mut write_half = write_half;
            loop {
                // N-M3: register interest in `notify` BEFORE checking `fired`
                // — tokio's documented safe pattern for closing the gap
                // between "we checked and it was false" and "we started
                // waiting", where a `notify_waiters()` landing in exactly
                // that gap would otherwise never wake us. `enable()` makes
                // this `Notified` count any notification from here on, even
                // ones that land before we actually `.await` it below.
                let notified = writer_death.notify.notified();
                tokio::pin!(notified);
                notified.as_mut().enable();
                if writer_death.fired.load(Ordering::SeqCst) {
                    break;
                }
                tokio::select! {
                    biased;
                    _ = notified.as_mut() => break,
                    maybe_bytes = write_rx.recv() => {
                        let Some(bytes) = maybe_bytes else { break };
                        let outcome = tokio::time::timeout(write_timeout, async {
                            write_half.write_all(&bytes).await?;
                            write_half.flush().await
                        })
                        .await;
                        match outcome {
                            Ok(Ok(())) => {}
                            Ok(Err(e)) => {
                                tracing::warn!(error = %e, "sin90: transport write failed; connection considered dead");
                                declare_dead(&writer_death, &writer_pending, &writer_semaphore, &writer_on_fatal);
                                break;
                            }
                            Err(_elapsed) => {
                                tracing::warn!(?write_timeout, "sin90: transport write timed out; connection considered dead");
                                declare_dead(&writer_death, &writer_pending, &writer_semaphore, &writer_on_fatal);
                                break;
                            }
                        }
                    }
                }
            }
            let _ = write_half.shutdown().await;
        });

        let reader_death = Arc::clone(&death);
        let reader_pending = Arc::clone(&pending);
        let reader_semaphore = Arc::clone(&semaphore);
        let reader_on_fatal = Arc::clone(&on_fatal);
        let reader_task = tokio::spawn(async move {
            let mut reader = BufReader::new(read_half);
            loop {
                // N-M3: same enable-before-check pattern as the writer loop.
                let notified = reader_death.notify.notified();
                tokio::pin!(notified);
                notified.as_mut().enable();
                if reader_death.fired.load(Ordering::SeqCst) {
                    break;
                }
                tokio::select! {
                    biased;
                    _ = notified.as_mut() => break,
                    frame_result = frame::read_frame(&mut reader) => {
                        match frame_result {
                            Ok(line) => match serde_json::from_slice::<Value>(&line) {
                                Ok(resp) => dispatch_response(&reader_pending, resp),
                                Err(e) => {
                                    tracing::warn!(error = %e, "sin90: transport received a non-JSON frame; connection considered dead");
                                    declare_dead(&reader_death, &reader_pending, &reader_semaphore, &reader_on_fatal);
                                    break;
                                }
                            },
                            Err(e) => {
                                tracing::debug!(error = %e, "sin90: transport read ended; connection considered dead");
                                declare_dead(&reader_death, &reader_pending, &reader_semaphore, &reader_on_fatal);
                                break;
                            }
                        }
                    }
                }
            }
        });

        Self {
            write_tx,
            pending,
            next_id: AtomicU64::new(1),
            semaphore,
            death,
            writer_task,
            reader_task,
        }
    }

    pub(crate) fn is_alive(&self) -> bool {
        !self.death.fired.load(Ordering::SeqCst)
    }

    /// Issue one call and await its response. The id is a per-`Transport`
    /// monotonic counter formatted as a string — unique for this
    /// connection's lifetime, which is all it needs to be (no reconnect, no
    /// surviving id to carry over).
    ///
    /// No production caller as of this commit (`KernelEventSink` uses
    /// [`Transport::call_with_slot_wait`] instead) — kept for a future
    /// fail-fast typed client (T3.2.1) and used throughout this module's own
    /// tests, which is most of what exercises `Transport` at all.
    #[allow(dead_code)]
    pub(crate) async fn call(&self, method: &str, params: Value) -> Result<Value, TransportError> {
        self.call_with_timeout(method, params, RESPONSE_TIMEOUT)
            .await
    }

    /// Same as [`Transport::call`] with an overridable response deadline —
    /// production code never needs this (`Transport::call` always uses
    /// [`RESPONSE_TIMEOUT`]); tests use it to exercise the timeout path
    /// without a 35-second sleep.
    #[allow(dead_code)]
    pub(crate) async fn call_with_timeout(
        &self,
        method: &str,
        params: Value,
        response_timeout: Duration,
    ) -> Result<Value, TransportError> {
        let (id, encoded) = self.encode_request(method, params)?;
        let permit = match Arc::clone(&self.semaphore).try_acquire_owned() {
            Ok(permit) => permit,
            // N-M3: `declare_dead` now closes the semaphore, so a dead
            // connection answers `NotSent` (accurate: definitely not going
            // anywhere) rather than `Busy` (which would suggest retrying
            // makes sense).
            Err(tokio::sync::TryAcquireError::Closed) => return Err(TransportError::NotSent),
            Err(tokio::sync::TryAcquireError::NoPermits) => return Err(TransportError::Busy),
        };
        self.send_and_await(id, encoded, permit, response_timeout)
            .await
    }

    /// Like [`Transport::call`], but waits up to `slot_wait` for an
    /// in-flight slot instead of failing immediately the moment all
    /// [`MAX_IN_FLIGHT_PER_CONNECTION`] are taken (M5/N-M1:
    /// `KernelEventSink` uses this via its own sub-quota gate — an event
    /// tolerates a brief wait better than being silently dropped on a short
    /// burst; nothing else needs the extra tolerance, so every other caller
    /// stays fail-fast via [`Transport::call`]). Giving up after `slot_wait`
    /// still reports [`TransportError::Busy`] — the meaning is the same ("no
    /// slot"), only how long it took to give up differs. N-M3: if the
    /// connection dies while this is waiting, `declare_dead`'s
    /// `semaphore.close()` wakes it immediately with
    /// [`TransportError::NotSent`] rather than riding out the rest of
    /// `slot_wait`.
    pub(crate) async fn call_with_slot_wait(
        &self,
        method: &str,
        params: Value,
        slot_wait: Duration,
    ) -> Result<Value, TransportError> {
        let (id, encoded) = self.encode_request(method, params)?;
        let permit = match tokio::time::timeout(
            slot_wait,
            Arc::clone(&self.semaphore).acquire_owned(),
        )
        .await
        {
            Ok(Ok(permit)) => permit,
            Ok(Err(_closed)) => return Err(TransportError::NotSent),
            Err(_elapsed) => return Err(TransportError::Busy),
        };
        self.send_and_await(id, encoded, permit, RESPONSE_TIMEOUT)
            .await
    }

    /// Serializes the envelope and applies M3's size check — shared by every
    /// `call*` entry point so the check runs identically, and BEFORE a
    /// semaphore permit is taken, regardless of which one is used.
    fn encode_request(
        &self,
        method: &str,
        params: Value,
    ) -> Result<(String, Vec<u8>), TransportError> {
        let id = format!("t-{}", self.next_id.fetch_add(1, Ordering::Relaxed));
        let req = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        let mut encoded = serde_json::to_vec(&req).expect("a json! Value always serializes");
        if encoded.len() > frame::MAX_FRAME_BYTES {
            return Err(TransportError::FrameTooLarge);
        }
        encoded.push(b'\n');
        Ok((id, encoded))
    }

    /// Registers the waiter, hands the encoded frame to the writer, and
    /// awaits the response (or `response_timeout`) — the tail shared by
    /// every `call*` entry point, once each has its own permit.
    async fn send_and_await(
        &self,
        id: String,
        encoded: Vec<u8>,
        permit: tokio::sync::OwnedSemaphorePermit,
        response_timeout: Duration,
    ) -> Result<Value, TransportError> {
        let (tx, rx) = oneshot::channel();
        {
            let mut guard = self.pending.lock().unwrap();
            if guard.closed {
                return Err(TransportError::NotSent);
            }
            match guard.map.entry(id.clone()) {
                Entry::Occupied(_) => return Err(TransportError::IdCollision),
                Entry::Vacant(v) => {
                    v.insert(tx);
                }
            }
        }

        // Holds the permit and, on drop before a normal completion, sends
        // `$/cancelRequest` and frees the pending-map slot.
        let _guard = CallGuard {
            id: id.clone(),
            pending: Arc::clone(&self.pending),
            write_tx: self.write_tx.clone(),
            _permit: permit,
        };

        if let Err(e) = self.write_tx.send(encoded).await {
            // Do NOT short-circuit here with `NotSent`: a `send` only fails
            // once the writer task has ended, which only happens through
            // `declare_dead` — and `declare_dead` already drained `pending`,
            // including this call's entry, sending it `ConnectionLost`
            // BEFORE the writer dropped its receiver (the very thing that
            // makes `send` start failing). So `rx` below already has the
            // right answer queued; returning early here would race it and
            // risk reporting the less accurate `NotSent` instead.
            tracing::debug!(error = %e, id, "sin90: transport write-queue send failed (connection already dying)");
        }

        match tokio::time::timeout(response_timeout, rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_recv_error)) => Err(TransportError::ConnectionLost),
            Err(_elapsed) => Err(TransportError::Timeout),
        }
    }
}

impl Drop for Transport {
    fn drop(&mut self) {
        self.writer_task.abort();
        self.reader_task.abort();
    }
}

#[cfg(test)]
impl Transport {
    fn available_permits(&self) -> usize {
        self.semaphore.available_permits()
    }
}

/// Frees the in-flight slot for one call and, if it had not already been
/// answered, tells the kernel to stop working on it.
struct CallGuard {
    id: String,
    pending: Pending,
    write_tx: mpsc::Sender<Vec<u8>>,
    _permit: tokio::sync::OwnedSemaphorePermit,
}

impl Drop for CallGuard {
    fn drop(&mut self) {
        // `dispatch_response`/`fail_all_pending` remove the entry BEFORE
        // sending a result, so by the time a normally-completed (or
        // already-failed) `call()` reaches this drop, `remove` here finds
        // nothing and sends no cancel. Only a call abandoned before any
        // answer arrived — future dropped, or timed out — still has an
        // entry here.
        let still_pending = self.pending.lock().unwrap().map.remove(&self.id).is_some();
        if still_pending {
            let notice = json!({
                "jsonrpc": "2.0",
                "method": CANCEL_METHOD,
                "params": { "id": self.id },
            });
            let mut bytes = serde_json::to_vec(&notice).expect("always serializes");
            bytes.push(b'\n');
            // Best-effort, non-blocking: `Drop` cannot `.await`, and a full
            // queue just means this cancel is silently skipped — the permit
            // is freed regardless, right below.
            let _ = self.write_tx.try_send(bytes);
        }
        // `_permit` drops here, releasing the semaphore slot.
    }
}

fn dispatch_response(pending: &Pending, resp: Value) {
    let Some(id) = resp.get("id").and_then(Value::as_str) else {
        tracing::warn!(
            ?resp,
            "sin90: transport received a frame with no string id; dropping"
        );
        return;
    };
    let sender = { pending.lock().unwrap().map.remove(id) };
    let Some(sender) = sender else {
        // Late response to an id this connection no longer has a waiter for
        // (typically: the caller cancelled it). Not an error — drop it.
        tracing::debug!(
            id,
            "sin90: transport dropped a response with no waiting caller"
        );
        return;
    };
    let result = if let Some(err) = resp.get("error") {
        Err(TransportError::Rpc(RpcErrorInfo::from_json(err)))
    } else {
        Ok(resp.get("result").cloned().unwrap_or(Value::Null))
    };
    let _ = sender.send(result);
}

/// Fails every still-pending call with `err` and marks the connection
/// closed — future `call()`s fail fast with [`TransportError::NotSent`]
/// instead of trying the (dead) wire. Safe to call more than once (a write
/// failure and a read failure racing each other): [`declare_dead`] already
/// guards this with `fired`, but the drain itself is also idempotent on its
/// own (a second call finds an empty map) should anything ever call it
/// directly.
fn fail_all_pending(pending: &Pending, err: TransportError) {
    let drained: Vec<_> = {
        let mut guard = pending.lock().unwrap();
        guard.closed = true;
        guard.map.drain().collect()
    };
    for (_, tx) in drained {
        let _ = tx.send(Err(err.clone()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    use tokio::io::{AsyncBufReadExt, BufReader as TokioBufReader};
    use tokio::net::UnixListener;

    // Short prefix deliberately: a Unix domain socket path must fit in
    // `sockaddr_un.sun_path` (macOS: 104 bytes total, including
    // `std::env::temp_dir()`'s own ~50-byte base).
    fn tempdir() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("s90t-{}", crate::core::ulid()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    async fn connected_pair() -> (UnixStream, UnixStream) {
        let dir = tempdir();
        let sock_path = dir.join("cb.sock");
        let listener = UnixListener::bind(&sock_path).unwrap();
        let client = UnixStream::connect(&sock_path).await.unwrap();
        let (server, _) = listener.accept().await.unwrap();
        (client, server)
    }

    fn counting_hook() -> (FatalHook, Arc<AtomicUsize>) {
        let count = Arc::new(AtomicUsize::new(0));
        let hook_count = Arc::clone(&count);
        let hook: FatalHook = Arc::new(move || {
            hook_count.fetch_add(1, Ordering::SeqCst);
        });
        (hook, count)
    }

    async fn read_json_line(reader: &mut TokioBufReader<tokio::net::unix::OwnedReadHalf>) -> Value {
        let mut buf = Vec::new();
        reader.read_until(b'\n', &mut buf).await.unwrap();
        serde_json::from_slice(&buf).unwrap()
    }

    async fn write_json_line(writer: &mut tokio::net::unix::OwnedWriteHalf, value: &Value) {
        let mut bytes = serde_json::to_vec(value).unwrap();
        bytes.push(b'\n');
        writer.write_all(&bytes).await.unwrap();
    }

    async fn wait_until(mut cond: impl FnMut() -> bool, timeout: Duration) {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            if cond() {
                return;
            }
            if tokio::time::Instant::now() >= deadline {
                panic!("condition did not become true within {timeout:?}");
            }
            tokio::task::yield_now().await;
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    #[tokio::test]
    async fn transport_concurrent_calls_out_of_order_responses_each_get_own_result() {
        let (client, server) = connected_pair().await;
        let (hook, _count) = counting_hook();
        let transport = Arc::new(Transport::spawn(client, hook));
        let (server_read, mut server_write) = server.into_split();
        let mut server_read = TokioBufReader::new(server_read);

        let t1 = Arc::clone(&transport);
        let call_a = tokio::spawn(async move { t1.call("method.a", json!({"who": "a"})).await });
        let req_a = read_json_line(&mut server_read).await;

        let t2 = Arc::clone(&transport);
        let call_b = tokio::spawn(async move { t2.call("method.b", json!({"who": "b"})).await });
        let req_b = read_json_line(&mut server_read).await;

        assert_eq!(req_a["method"], "method.a");
        assert_eq!(req_b["method"], "method.b");
        let id_a = req_a["id"].as_str().unwrap().to_string();
        let id_b = req_b["id"].as_str().unwrap().to_string();
        assert_ne!(id_a, id_b, "each call must get a unique id");

        // Answer B FIRST, then A — deliberately out of request order.
        write_json_line(
            &mut server_write,
            &json!({"jsonrpc": "2.0", "id": id_b, "result": {"who": "b"}}),
        )
        .await;
        write_json_line(
            &mut server_write,
            &json!({"jsonrpc": "2.0", "id": id_a, "result": {"who": "a"}}),
        )
        .await;

        let result_a = call_a.await.unwrap().unwrap();
        let result_b = call_b.await.unwrap().unwrap();
        assert_eq!(
            result_a,
            json!({"who": "a"}),
            "call A must get call A's own result"
        );
        assert_eq!(
            result_b,
            json!({"who": "b"}),
            "call B must get call B's own result"
        );
    }

    #[tokio::test]
    async fn transport_65th_in_flight_call_is_rejected_busy() {
        let (client, server) = connected_pair().await;
        let (hook, _count) = counting_hook();
        let transport = Arc::new(Transport::spawn(client, hook));
        let (server_read, _server_write) = server.into_split();
        tokio::spawn(async move {
            let mut reader = TokioBufReader::new(server_read);
            let mut buf = Vec::new();
            loop {
                buf.clear();
                if reader.read_until(b'\n', &mut buf).await.unwrap_or(0) == 0 {
                    break;
                }
            }
        });

        let mut handles = Vec::new();
        for i in 0..MAX_IN_FLIGHT_PER_CONNECTION {
            let transport = Arc::clone(&transport);
            handles.push(tokio::spawn(async move {
                transport.call(&format!("method.{i}"), json!({})).await
            }));
        }
        wait_until(
            || transport.available_permits() == 0,
            Duration::from_secs(1),
        )
        .await;

        let busy = transport.call("method.65th", json!({})).await;
        assert!(
            matches!(busy, Err(TransportError::Busy)),
            "the 65th concurrent call must be rejected Busy immediately, got {busy:?}"
        );

        for h in handles {
            h.abort();
        }
    }

    #[tokio::test]
    async fn transport_cancel_on_drop_releases_slot_and_writes_exact_cancel_frame() {
        let (client, server) = connected_pair().await;
        let (hook, _count) = counting_hook();
        let transport = Arc::new(Transport::spawn(client, hook));
        let (server_read, _server_write) = server.into_split();
        let mut server_read = TokioBufReader::new(server_read);

        let t = Arc::clone(&transport);
        let handle = tokio::spawn(async move { t.call("routine.upsert", json!({"a": 1})).await });

        let request_line = read_json_line(&mut server_read).await;
        assert_eq!(request_line["method"], "routine.upsert");
        // The very first call on a fresh `Transport` always gets id "t-1" —
        // ground truth for the literal comparison below, independent of our
        // own serializer.
        assert_eq!(request_line["id"], "t-1");
        assert_eq!(
            transport.available_permits(),
            MAX_IN_FLIGHT_PER_CONNECTION - 1,
            "the call holds its slot until answered or cancelled"
        );

        handle.abort();
        let _ = handle.await;

        let cancel_frame = read_frame_bytes(&mut server_read).await;
        // Deliberately NOT derived from the same `json!`/serializer call the
        // production code uses — a literal, so this test cannot be made to
        // pass merely by keeping two identical constructions in sync.
        let expected: Value = serde_json::from_str(
            r#"{"jsonrpc":"2.0","method":"$/cancelRequest","params":{"id":"t-1"}}"#,
        )
        .unwrap();
        let actual: Value = serde_json::from_slice(&cancel_frame).unwrap();
        assert_eq!(
            actual, expected,
            "cancel frame must match the SPEC wire shape exactly"
        );
        assert!(
            actual.get("id").is_none(),
            "a notification must not carry a top-level id"
        );

        wait_until(
            || transport.available_permits() == MAX_IN_FLIGHT_PER_CONNECTION,
            Duration::from_secs(1),
        )
        .await;
    }

    #[tokio::test]
    async fn transport_cancel_is_not_sent_when_the_call_completes_normally() {
        let (client, server) = connected_pair().await;
        let (hook, _count) = counting_hook();
        let transport = Arc::new(Transport::spawn(client, hook));
        let (server_read, mut server_write) = server.into_split();
        let mut server_read = TokioBufReader::new(server_read);

        let t = Arc::clone(&transport);
        let handle = tokio::spawn(async move { t.call("routine.upsert", json!({})).await });
        let request_line = read_json_line(&mut server_read).await;
        let id = request_line["id"].as_str().unwrap().to_string();

        write_json_line(
            &mut server_write,
            &json!({"jsonrpc": "2.0", "id": id, "result": {"ok": true}}),
        )
        .await;
        let result = handle.await.unwrap().unwrap();
        assert_eq!(result, json!({"ok": true}));

        // Positive control for the cancel test above: a normally-completed
        // call must NOT also write a cancel frame. Prove it by sending one
        // more real call and checking the kernel sees THAT request next —
        // not a leftover `$/cancelRequest` for the first one.
        let t2 = Arc::clone(&transport);
        let handle2 = tokio::spawn(async move { t2.call("routine.other", json!({})).await });
        let next_line = read_json_line(&mut server_read).await;
        assert_eq!(
            next_line["method"], "routine.other",
            "no stray cancel frame must appear between the two calls"
        );
        let id2 = next_line["id"].as_str().unwrap().to_string();
        write_json_line(
            &mut server_write,
            &json!({"jsonrpc": "2.0", "id": id2, "result": {}}),
        )
        .await;
        handle2.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn transport_late_response_after_cancel_is_dropped_and_transport_keeps_working() {
        let (client, server) = connected_pair().await;
        let (hook, _count) = counting_hook();
        let transport = Arc::new(Transport::spawn(client, hook));
        let (server_read, mut server_write) = server.into_split();
        let mut server_read = TokioBufReader::new(server_read);

        let t = Arc::clone(&transport);
        let handle = tokio::spawn(async move { t.call("slow.method", json!({})).await });
        let request_line = read_json_line(&mut server_read).await;
        let id = request_line["id"].as_str().unwrap().to_string();

        handle.abort();
        let _ = handle.await;
        let _cancel_frame = read_json_line(&mut server_read).await;

        // The kernel answers anyway, after the caller already gave up.
        write_json_line(
            &mut server_write,
            &json!({"jsonrpc": "2.0", "id": id, "result": {"ok": true}}),
        )
        .await;

        // Positive control: the transport is still genuinely usable — issue
        // a brand new call and let it complete normally.
        let t2 = Arc::clone(&transport);
        let handle2 = tokio::spawn(async move { t2.call("after.stale", json!({})).await });
        let next = read_json_line(&mut server_read).await;
        assert_eq!(next["method"], "after.stale");
        let id2 = next["id"].as_str().unwrap().to_string();
        write_json_line(
            &mut server_write,
            &json!({"jsonrpc": "2.0", "id": id2, "result": {"still": "alive"}}),
        )
        .await;
        let result = handle2.await.unwrap().unwrap();
        assert_eq!(result, json!({"still": "alive"}));
    }

    #[tokio::test]
    async fn transport_disconnect_in_flight_call_gets_connection_lost_and_never_retries() {
        // Uses a real `UnixListener` (not `connected_pair`'s throwaway one)
        // so the "never reconnects" half of this test can assert against it
        // directly, rather than merely asserting on the caller's error (L-3:
        // the old name claimed "kernel sees exactly one request" without
        // actually checking either half of that).
        let dir = tempdir();
        let sock_path = dir.join("cb.sock");
        let listener = UnixListener::bind(&sock_path).unwrap();
        let client = UnixStream::connect(&sock_path).await.unwrap();
        let (server, _) = listener.accept().await.unwrap();

        let (hook, count) = counting_hook();
        let transport = Arc::new(Transport::spawn(client, hook));
        let (server_read, server_write) = server.into_split();
        let mut server_read = TokioBufReader::new(server_read);

        let t = Arc::clone(&transport);
        let handle = tokio::spawn(async move { t.call("routine.upsert", json!({})).await });
        let request_line = read_json_line(&mut server_read).await;
        assert_eq!(request_line["method"], "routine.upsert");

        // Real assertion #1: exactly one frame arrives for this call — no
        // retry on the SAME connection before it goes away.
        let second_frame_on_same_connection =
            tokio::time::timeout(Duration::from_millis(100), read_json_line(&mut server_read))
                .await;
        assert!(
            second_frame_on_same_connection.is_err(),
            "the kernel must see exactly one frame for this call, not a retry"
        );

        // The kernel hangs up without answering.
        drop(server_write);
        drop(server_read);

        let result = handle.await.unwrap();
        assert!(
            matches!(result, Err(TransportError::ConnectionLost)),
            "an in-flight call must get ConnectionLost on disconnect, got {result:?}"
        );
        wait_until(|| !transport.is_alive(), Duration::from_secs(1)).await;
        wait_until(|| count.load(Ordering::SeqCst) == 1, Duration::from_secs(1)).await;

        // Real assertion #2: no reconnect is ever attempted — nothing shows
        // up in the listener's backlog (matches the kernel's own D1: this
        // generation's connection is over, and it is not a second `Transport`
        // job to redial).
        let second_connection =
            tokio::time::timeout(Duration::from_millis(200), listener.accept()).await;
        assert!(
            second_connection.is_err(),
            "Transport must never attempt to reconnect"
        );
    }

    #[tokio::test]
    async fn transport_call_after_close_fails_fast_as_not_sent() {
        let (client, server) = connected_pair().await;
        let (hook, _count) = counting_hook();
        let transport = Arc::new(Transport::spawn(client, hook));
        let (server_read, server_write) = server.into_split();

        // Kill the connection first.
        drop(server_write);
        drop(server_read);
        wait_until(|| !transport.is_alive(), Duration::from_secs(1)).await;

        let started = tokio::time::Instant::now();
        let result = transport.call("routine.upsert", json!({})).await;
        assert!(
            matches!(result, Err(TransportError::NotSent)),
            "a call on an already-dead connection must fail fast as NotSent, got {result:?}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "NotSent must be fast — no timeout, no blocking on a dead wire"
        );
    }

    #[tokio::test]
    async fn transport_frame_too_large_is_rejected_before_reaching_the_writer() {
        let (client, server) = connected_pair().await;
        let (hook, _count) = counting_hook();
        let transport = Transport::spawn(client, hook);
        let (server_read, _server_write) = server.into_split();
        let mut server_read = TokioBufReader::new(server_read);

        let huge = "x".repeat(frame::MAX_FRAME_BYTES + 1);
        let result = transport
            .call("routine.upsert", json!({"huge": huge}))
            .await;
        assert!(
            matches!(result, Err(TransportError::FrameTooLarge)),
            "got {result:?}"
        );

        // Nothing must have reached the wire at all.
        let nothing =
            tokio::time::timeout(Duration::from_millis(100), read_json_line(&mut server_read))
                .await;
        assert!(
            nothing.is_err(),
            "an oversized call must never reach the writer/wire"
        );
    }

    #[tokio::test]
    async fn transport_frame_of_exactly_max_bytes_is_accepted() {
        // L-4: positive control for the size check — catches an off-by-one
        // mutation (`>` accidentally becoming `>=`) that the "MAX+1 is
        // rejected" test alone cannot: that test would still pass under such
        // a mutation, since MAX+1 is still `>= MAX_FRAME_BYTES`.
        let (client, server) = connected_pair().await;
        let (hook, _count) = counting_hook();
        let transport = Transport::spawn(client, hook);
        let (server_read, mut server_write) = server.into_split();
        let mut server_read = TokioBufReader::new(server_read);

        // Reproduce the exact envelope `Transport::call` builds (same key
        // set/order, same id — "t-1" is always the first call's id on a
        // fresh `Transport`) to compute the padding that lands the encoded
        // frame at precisely `MAX_FRAME_BYTES`.
        let method = "routine.upsert";
        let id = "t-1";
        let base = serde_json::to_vec(&json!({
            "jsonrpc": "2.0", "id": id, "method": method, "params": {"pad": ""}
        }))
        .unwrap();
        let pad_len = frame::MAX_FRAME_BYTES - base.len();
        let pad = "x".repeat(pad_len);
        let envelope_len = base.len() + pad_len;
        assert_eq!(
            envelope_len,
            frame::MAX_FRAME_BYTES,
            "test construction bug: padding must land exactly on the limit"
        );

        let call = transport.call(method, json!({"pad": pad}));
        // Bounded, not `tokio::join!` alone: if the size check ever
        // regresses to reject exactly-MAX (the mutation this test exists to
        // catch), `call` resolves immediately with `FrameTooLarge` and
        // nothing is ever written to the wire — `read_json_line` would then
        // wait forever with nothing to time out on. Wrapping the whole pair
        // turns that into a clean, fast test failure instead of a hang.
        let joined = tokio::time::timeout(Duration::from_secs(2), async {
            tokio::join!(
                async {
                    let line = read_json_line(&mut server_read).await;
                    write_json_line(
                        &mut server_write,
                        &json!({"jsonrpc": "2.0", "id": line["id"], "result": {}}),
                    )
                    .await;
                    line
                },
                call,
            )
        })
        .await;
        let (request_line, call_result) = joined.expect(
            "exactly-MAX call must reach the wire and round-trip — if this timed out, the size \
             check likely regressed to reject the boundary case (see the mutation this test \
             exists to catch)",
        );
        assert_eq!(request_line["method"], method);
        assert_eq!(
            call_result.unwrap(),
            json!({}),
            "a call whose frame is exactly MAX_FRAME_BYTES must be accepted, not rejected"
        );
    }

    #[tokio::test]
    async fn transport_response_timeout_sends_cancel_and_returns_timeout_error() {
        let (client, server) = connected_pair().await;
        let (hook, _count) = counting_hook();
        let transport = Transport::spawn(client, hook);
        let (server_read, _server_write) = server.into_split();
        let mut server_read = TokioBufReader::new(server_read);

        let request_line_fut = read_json_line(&mut server_read);
        let call_fut =
            transport.call_with_timeout("slow.method", json!({}), Duration::from_millis(50));
        let (request_line, result) = tokio::join!(request_line_fut, call_fut);
        assert_eq!(request_line["method"], "slow.method");
        assert!(
            matches!(result, Err(TransportError::Timeout)),
            "got {result:?}"
        );

        let cancel_frame = read_json_line(&mut server_read).await;
        assert_eq!(cancel_frame["method"], CANCEL_METHOD);
        assert!(cancel_frame.get("id").is_none());
    }

    #[tokio::test]
    async fn transport_write_timeout_triggers_on_fatal_once_and_in_flight_calls_get_connection_lost(
    ) {
        // N-M4①: force the writer's own `WRITE_TIMEOUT` path (not the
        // reader's EOF/bad-frame paths, already covered elsewhere) — the
        // fake kernel never reads at all, and enough concurrent large
        // payloads are queued that the OS socket buffers on both ends fill
        // up well before `write_timeout` elapses, so `write_all` genuinely
        // blocks past it rather than merely being slow.
        let (client, server) = connected_pair().await;
        let (hook, count) = counting_hook();
        let config = TransportConfig {
            write_timeout: Duration::from_millis(50),
        };
        let transport = Arc::new(Transport::spawn_with_config(client, hook, config));
        // Deliberately never read from `server` — that is the point.
        let _server = server;

        let big = "x".repeat(200_000);
        let mut handles = Vec::new();
        for i in 0..20 {
            let t = Arc::clone(&transport);
            let payload = big.clone();
            handles.push(tokio::spawn(async move {
                t.call(&format!("flood.{i}"), json!({"data": payload}))
                    .await
            }));
        }

        let mut saw_connection_lost = false;
        for h in handles {
            match h.await.unwrap() {
                Err(TransportError::ConnectionLost) => saw_connection_lost = true,
                Err(TransportError::NotSent) => {}
                other => panic!(
                    "expected ConnectionLost/NotSent once the write times out, got {other:?}"
                ),
            }
        }
        assert!(
            saw_connection_lost,
            "at least one call must have genuinely been in flight when the write timed out"
        );
        wait_until(|| count.load(Ordering::SeqCst) == 1, Duration::from_secs(2)).await;
    }

    #[tokio::test]
    async fn transport_declare_dead_fires_on_fatal_exactly_once_under_concurrent_callers() {
        // N-M4②: a deterministic replacement for the old "drop both socket
        // halves and hope reader/writer race, then sleep and hope the sleep
        // was long enough" test. `declare_dead` is the ONE place that can
        // fire `on_fatal` — this drives it directly from many concurrent
        // tasks (which is the shape of the real race: reader and writer
        // tasks calling it at close to the same instant) and asserts the
        // count, not a timing-dependent absence of a second call.
        let (hook, count) = counting_hook();
        let death = Arc::new(DeathSignal {
            fired: AtomicBool::new(false),
            notify: Notify::new(),
        });
        let pending: Pending = Arc::new(StdMutex::new(PendingState {
            closed: false,
            map: HashMap::new(),
        }));
        let semaphore = Arc::new(Semaphore::new(MAX_IN_FLIGHT_PER_CONNECTION));

        let mut handles = Vec::new();
        for _ in 0..100 {
            let death = Arc::clone(&death);
            let pending = Arc::clone(&pending);
            let semaphore = Arc::clone(&semaphore);
            let hook = Arc::clone(&hook);
            handles.push(tokio::spawn(async move {
                declare_dead(&death, &pending, &semaphore, &hook);
            }));
        }
        for h in handles {
            h.await.unwrap();
        }

        assert_eq!(
            count.load(Ordering::SeqCst),
            1,
            "on_fatal must fire exactly once even under many concurrent callers"
        );
        assert!(
            semaphore.is_closed(),
            "declare_dead must close the semaphore so a waiting call_with_slot_wait wakes immediately"
        );
    }

    #[tokio::test]
    async fn transport_on_fatal_is_triggered_by_a_non_json_frame_from_the_kernel() {
        let (client, server) = connected_pair().await;
        let (hook, count) = counting_hook();
        let transport = Arc::new(Transport::spawn(client, hook));
        let (server_read, mut server_write) = server.into_split();
        let mut server_read = TokioBufReader::new(server_read);

        let t = Arc::clone(&transport);
        let handle = tokio::spawn(async move { t.call("routine.upsert", json!({})).await });
        let _request_line = read_json_line(&mut server_read).await;

        server_write.write_all(b"not json at all\n").await.unwrap();

        let result = handle.await.unwrap();
        assert!(
            matches!(result, Err(TransportError::ConnectionLost)),
            "got {result:?}"
        );
        wait_until(|| count.load(Ordering::SeqCst) == 1, Duration::from_secs(1)).await;
    }

    #[tokio::test]
    async fn transport_on_fatal_is_triggered_by_an_oversized_incoming_frame() {
        let (client, server) = connected_pair().await;
        let (hook, count) = counting_hook();
        let transport = Arc::new(Transport::spawn(client, hook));
        let (server_read, mut server_write) = server.into_split();
        let mut server_read = TokioBufReader::new(server_read);

        let t = Arc::clone(&transport);
        let handle = tokio::spawn(async move { t.call("routine.upsert", json!({})).await });
        let _request_line = read_json_line(&mut server_read).await;

        let mut huge = vec![b'x'; frame::MAX_FRAME_BYTES + 1];
        huge.push(b'\n');
        server_write.write_all(&huge).await.unwrap();

        let result = handle.await.unwrap();
        assert!(
            matches!(result, Err(TransportError::ConnectionLost)),
            "got {result:?}"
        );
        wait_until(|| count.load(Ordering::SeqCst) == 1, Duration::from_secs(1)).await;
    }

    #[tokio::test]
    async fn transport_call_with_slot_wait_succeeds_once_a_slot_frees_up() {
        let (client, server) = connected_pair().await;
        let (hook, _count) = counting_hook();
        let transport = Arc::new(Transport::spawn(client, hook));
        let (server_read, mut server_write) = server.into_split();
        let mut server_read = TokioBufReader::new(server_read);

        // Saturate all 64 slots with calls the fake kernel never answers.
        let mut handles = Vec::new();
        let mut ids = Vec::new();
        for i in 0..MAX_IN_FLIGHT_PER_CONNECTION {
            let t = Arc::clone(&transport);
            handles.push(tokio::spawn(async move {
                t.call(&format!("method.{i}"), json!({})).await
            }));
        }
        for _ in 0..MAX_IN_FLIGHT_PER_CONNECTION {
            ids.push(
                read_json_line(&mut server_read).await["id"]
                    .as_str()
                    .unwrap()
                    .to_string(),
            );
        }
        wait_until(
            || transport.available_permits() == 0,
            Duration::from_secs(1),
        )
        .await;

        // A slot-waiting call, parked behind the full semaphore.
        let t = Arc::clone(&transport);
        let waiter = tokio::spawn(async move {
            t.call_with_slot_wait("event.emit", json!({}), Duration::from_secs(2))
                .await
        });
        // L-5: give the waiter task a moment to actually start (and block
        // on) `acquire_owned`, THEN assert it has NOT finished yet — proving
        // it is genuinely waiting for a slot, not that it happened to run
        // fast enough that the timing of the assertion below wouldn't have
        // caught a bug either way.
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(
            !waiter.is_finished(),
            "the waiter must still be blocked on a slot before any is freed"
        );

        // Free exactly one slot by answering one in-flight call.
        write_json_line(
            &mut server_write,
            &json!({"jsonrpc": "2.0", "id": ids[0], "result": {}}),
        )
        .await;

        let waiter_request = read_json_line(&mut server_read).await;
        assert_eq!(waiter_request["method"], "event.emit");
        write_json_line(
            &mut server_write,
            &json!({"jsonrpc": "2.0", "id": waiter_request["id"], "result": {"ok": true}}),
        )
        .await;
        let result = waiter.await.unwrap();
        assert_eq!(result.unwrap(), json!({"ok": true}));

        for h in handles {
            h.abort();
        }
    }

    #[tokio::test]
    async fn transport_call_with_slot_wait_times_out_to_busy_when_no_slot_frees() {
        let (client, server) = connected_pair().await;
        let (hook, _count) = counting_hook();
        let transport = Arc::new(Transport::spawn(client, hook));
        let (server_read, _server_write) = server.into_split();
        tokio::spawn(async move {
            let mut reader = TokioBufReader::new(server_read);
            let mut buf = Vec::new();
            loop {
                buf.clear();
                if reader.read_until(b'\n', &mut buf).await.unwrap_or(0) == 0 {
                    break;
                }
            }
        });

        let mut handles = Vec::new();
        for i in 0..MAX_IN_FLIGHT_PER_CONNECTION {
            let t = Arc::clone(&transport);
            handles.push(tokio::spawn(async move {
                t.call(&format!("method.{i}"), json!({})).await
            }));
        }
        wait_until(
            || transport.available_permits() == 0,
            Duration::from_secs(1),
        )
        .await;

        let started = tokio::time::Instant::now();
        let result = transport
            .call_with_slot_wait("event.emit", json!({}), Duration::from_millis(50))
            .await;
        assert!(
            matches!(result, Err(TransportError::Busy)),
            "got {result:?}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "must give up around the requested wait, not hang"
        );

        for h in handles {
            h.abort();
        }
    }

    async fn read_frame_bytes(
        reader: &mut TokioBufReader<tokio::net::unix::OwnedReadHalf>,
    ) -> Vec<u8> {
        let mut buf = Vec::new();
        reader.read_until(b'\n', &mut buf).await.unwrap();
        if buf.last() == Some(&b'\n') {
            buf.pop();
        }
        buf
    }
}
