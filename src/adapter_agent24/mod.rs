//! The ONLY module that knows Agent24 exists (design §5.2).
//!
//! Reads `A24_DATA_DIR` / `A24_CALLBACK_SOCK` / `A24_HANDSHAKE_TOKEN` /
//! `A24_LISTEN_FD` (the real, `me3f_blackbox.rs`-verified contract, recorded
//! in `docs/STATUS.md`); runs the `initialize` handshake over the callback
//! Unix socket; accepts HTTP on the kernel-bound listener fd; wires
//! [`crate::http::EventSink`] to `_a24/events/emit` over that same socket.
//!
//! `agent24-os-sdk` (Agent24's T13) does not exist yet (design §7.2) — this is
//! the ~200 lines Sin90 writes itself until it does. Only this file needs to
//! change when the SDK lands.

use std::os::unix::io::FromRawFd;
use std::sync::Arc;

use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use tokio::io::BufReader;
use tokio::net::UnixStream;
use tokio::sync::Mutex;

use crate::http::EventSink;

mod frame;

/// Upper bound on one callback round trip (reconnect, or write + read the
/// reply). A kernel that accepts but never answers must not wedge the event
/// worker — and with it every later event — forever.
const DEFAULT_IO_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
/// Events waiting for the worker. Past this, new events are dropped and
/// counted rather than buffered without bound.
const DEFAULT_EVENT_QUEUE: usize = 1024;

/// This module's declared protocol range. `min == max == 1`: Sin90 speaks
/// exactly the one version `me3f_blackbox.rs` exercises; there is nothing yet
/// to negotiate a RANGE over.
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

/// A live callback connection, past the `initialize` handshake. `conn` is
/// behind a `Mutex` because every subsequent call (only `_a24/events/emit` in
/// M0) is a full request-then-read-the-matching-response round trip on the
/// SAME socket — serializing access here is simpler and sufficient at M0's
/// call volume than a background reader + per-request channel; if
/// `agent24-os-sdk` supersedes this file (design §7.2), that concurrency
/// design is its call to make, not this one's to anticipate.
///
/// `conn` is `None` exactly when the last known state of the socket is
/// "desynced or closed" — a read/write error, a frame that failed to parse,
/// or one that arrived with a `FrameTooLong`/id-mismatch shape we don't know
/// how to keep reading past. Leaving a `BufReader` in place after any of
/// those would mean every later `emit()` keeps reading from the wrong offset
/// in the stream forever, silently warning on each call until the process is
/// restarted — `emit()` reconnects (one fresh `initialize`) instead of
/// reusing a connection it can no longer trust the framing of.
pub struct CallbackChannel {
    conn: Mutex<Option<BufReader<UnixStream>>>,
    next_id: std::sync::atomic::AtomicU64,
    sock_path: std::path::PathBuf,
    module: String,
    manifest_bytes: Vec<u8>,
    auth_token: String,
    io_timeout: std::time::Duration,
}

impl CallbackChannel {
    /// Connect and run `initialize`. Returns the channel plus the kernel's
    /// `Offer` (which methods this connection may call).
    pub async fn handshake(
        sock_path: &std::path::Path,
        module: &str,
        manifest_bytes: &[u8],
        auth_token: &str,
    ) -> Result<(Self, Vec<String>), AdapterError> {
        let (reader, provides) =
            connect_and_initialize(sock_path, module, manifest_bytes, auth_token).await?;
        Ok((
            Self {
                conn: Mutex::new(Some(reader)),
                next_id: std::sync::atomic::AtomicU64::new(2), // id "1" was the handshake
                sock_path: sock_path.to_path_buf(),
                module: module.to_string(),
                manifest_bytes: manifest_bytes.to_vec(),
                auth_token: auth_token.to_string(),
                io_timeout: DEFAULT_IO_TIMEOUT,
            },
            provides,
        ))
    }

    /// Call `_a24/events/emit`. Best-effort: a callback failure is logged and
    /// swallowed, same posture as the ported kernel handlers' "no sink granted
    /// -> degrade, don't fail the mutation" (design §5.3) — a Sin90 write must
    /// not fail because the event side-channel hiccuped. On a desynced or
    /// closed connection, reconnects once before giving up on this call —
    /// see the struct doc for why reusing a broken `conn` isn't an option.
    pub async fn emit(&self, kind: &str, payload: Map<String, Value>) {
        let mut conn = self.conn.lock().await;
        if conn.is_none() {
            let reconnect = connect_and_initialize(
                &self.sock_path,
                &self.module,
                &self.manifest_bytes,
                &self.auth_token,
            );
            match tokio::time::timeout(self.io_timeout, reconnect).await {
                Ok(Ok((reader, _provides))) => *conn = Some(reader),
                Ok(Err(e)) => {
                    tracing::warn!(error = %e, kind, "sin90: could not reconnect to emit events/emit");
                    return;
                }
                Err(_) => {
                    tracing::warn!(kind, "sin90: reconnect for events/emit timed out");
                    return;
                }
            }
        }
        let id = self
            .next_id
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            .to_string();
        let req = json!({
            "jsonrpc": "2.0",
            "method": "_a24/events/emit",
            "id": id,
            "params": { "kind": kind, "payload": payload },
        });
        // From here on, any error means the stream's framing can no longer be
        // trusted (we don't know how much of a frame the kernel received, or
        // whether the reader's position matches a frame boundary) — drop the
        // connection so the NEXT `emit()` reconnects rather than continuing
        // to read a desynced stream. A timeout counts: the late reply would
        // otherwise be read as the answer to the next request.
        let reader = conn.as_mut().expect("just ensured Some above");
        let round_trip = async {
            frame::write_frame(reader.get_mut(), &req).await?;
            frame::read_frame(reader).await
        };
        let line = match tokio::time::timeout(self.io_timeout, round_trip).await {
            Ok(Ok(line)) => line,
            Ok(Err(e)) => {
                tracing::warn!(error = %e, kind, "sin90: events/emit round trip failed");
                *conn = None;
                return;
            }
            Err(_) => {
                tracing::warn!(kind, "sin90: events/emit timed out");
                *conn = None;
                return;
            }
        };
        match serde_json::from_slice::<Value>(&line) {
            Ok(resp) if resp["id"].as_str() != Some(id.as_str()) => {
                tracing::warn!(kind, sent = %id, got = %resp["id"], "sin90: events/emit reply id mismatch");
                *conn = None;
            }
            Ok(resp) => {
                if let Some(err) = resp.get("error") {
                    tracing::warn!(kind, ?err, "sin90: events/emit rejected by kernel");
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, kind, "sin90: events/emit response was not valid JSON");
                *conn = None;
            }
        }
    }
}

/// Connect to `sock_path` and run the `initialize` exchange, shared by the
/// initial [`CallbackChannel::handshake`] and every later reconnect inside
/// [`CallbackChannel::emit`] — one place that speaks the wire format.
async fn connect_and_initialize(
    sock_path: &std::path::Path,
    module: &str,
    manifest_bytes: &[u8],
    auth_token: &str,
) -> Result<(BufReader<UnixStream>, Vec<String>), AdapterError> {
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

    Ok((reader, provides))
}

/// Adapts [`CallbackChannel`] to [`crate::http::EventSink`]. `http` never
/// sees this type — only the trait.
///
/// One worker drains a bounded queue, so events reach the kernel in the order
/// they were emitted and a stalled kernel costs at most the queue, not one
/// suspended task per mutation (Codex 2026-09-22 review, Medium #6). When the
/// queue is full the event is dropped and counted — events are best-effort by
/// design (§5.3), a committed write must not wait on them.
pub struct KernelEventSink {
    tx: tokio::sync::mpsc::Sender<(String, Map<String, Value>)>,
    dropped: Arc<std::sync::atomic::AtomicU64>,
}

impl KernelEventSink {
    /// Must be called inside a Tokio runtime (spawns the worker).
    pub fn spawn(chan: Arc<CallbackChannel>) -> Self {
        Self::with_capacity(chan, DEFAULT_EVENT_QUEUE)
    }

    pub fn with_capacity(chan: Arc<CallbackChannel>, capacity: usize) -> Self {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<(String, Map<String, Value>)>(capacity);
        tokio::spawn(async move {
            while let Some((kind, payload)) = rx.recv().await {
                chan.emit(&kind, payload).await;
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
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

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

        let (_chan, provides) = CallbackChannel::handshake(&sock_path, "sin90", manifest, token)
            .await
            .unwrap();
        assert_eq!(provides, vec!["_a24/events/".to_string()]);
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

        let result = CallbackChannel::handshake(&sock_path, "sin90", b"x", "wrong-token").await;
        let Err(err) = result else {
            panic!("expected the handshake to be refused");
        };
        assert!(matches!(err, AdapterError::HandshakeRefused(_)));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn emit_clears_the_connection_when_the_server_hangs_up() {
        // Regression: before this fix, a read/write failure inside `emit()`
        // left the old `BufReader` in place — every later `emit()` kept
        // reading the same dead stream and silently warned forever, with no
        // way back short of restarting the process. After this fix, any
        // failure clears `conn`, so the NEXT `emit()` reconnects instead.
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
            assert_eq!(req["method"], "initialize");
            let resp = json!({
                "jsonrpc": "2.0", "id": req["id"],
                "result": { "protocol_version": 1, "offer": { "provides": ["_a24/events/"] } }
            });
            let mut bytes = serde_json::to_vec(&resp).unwrap();
            bytes.push(b'\n');
            reader.get_mut().write_all(&bytes).await.unwrap();
            // Hang up immediately after the handshake, before any emit
            // request can arrive — the dropped `reader` closes the socket.
        });

        let (chan, _provides) = CallbackChannel::handshake(&sock_path, "sin90", manifest, token)
            .await
            .unwrap();
        server.await.unwrap();

        chan.emit("test.orphaned", Map::new()).await;
        assert!(
            chan.conn.lock().await.is_none(),
            "emit() against a hung-up connection must clear `conn`, not leave a desynced reader in place for the next call to reuse"
        );
    }

    /// A kernel that completes the handshake, then reads every later frame
    /// and never answers — reporting each received method on `seen`.
    async fn silent_kernel(
        sock_path: &std::path::Path,
    ) -> tokio::sync::mpsc::UnboundedReceiver<String> {
        let listener = tokio::net::UnixListener::bind(sock_path).unwrap();
        let (seen_tx, seen) = tokio::sync::mpsc::unbounded_channel();
        tokio::spawn(async move {
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
            loop {
                buf.clear();
                if reader.read_until(b'\n', &mut buf).await.unwrap_or(0) == 0 {
                    return;
                }
                let req: Value = serde_json::from_slice(&buf).unwrap();
                let _ = seen_tx.send(req["method"].as_str().unwrap_or_default().to_string());
            }
        });
        seen
    }

    /// Medium #6: a kernel that accepts but never replies used to hold the
    /// connection mutex forever. The round trip is now bounded and the
    /// connection is dropped so a late reply can't be misread later.
    #[tokio::test]
    async fn emit_times_out_and_drops_the_connection_when_the_kernel_never_replies() {
        let dir = tempdir();
        let sock_path = dir.join("cb.sock");
        let _seen = silent_kernel(&sock_path).await;
        let (mut chan, _) = CallbackChannel::handshake(&sock_path, "sin90", b"x", "t")
            .await
            .unwrap();
        chan.io_timeout = std::time::Duration::from_millis(100);
        let started = std::time::Instant::now();
        chan.emit("test.silent", Map::new()).await;
        assert!(
            started.elapsed() < std::time::Duration::from_secs(2),
            "emit must give up after its timeout, took {:?}",
            started.elapsed()
        );
        assert!(chan.conn.lock().await.is_none());
    }

    /// Medium #6: every mutation used to spawn its own task, each waiting
    /// behind the stuck one. Now one worker and a bounded queue: overflow is
    /// dropped and counted.
    #[tokio::test]
    async fn event_queue_is_bounded_and_counts_drops_while_the_kernel_is_stuck() {
        let dir = tempdir();
        let sock_path = dir.join("cb.sock");
        let mut seen = silent_kernel(&sock_path).await;
        let (chan, _) = CallbackChannel::handshake(&sock_path, "sin90", b"x", "t")
            .await
            .unwrap();
        let sink = KernelEventSink::with_capacity(Arc::new(chan), 4);

        // First event: wait until the kernel has it, so the worker is known
        // to be parked inside that (never-answered) round trip.
        sink.emit("e0", Map::new());
        assert_eq!(seen.recv().await.as_deref(), Some("_a24/events/emit"));

        for i in 1..=14 {
            sink.emit(&format!("e{i}"), Map::new());
        }
        assert_eq!(sink.dropped(), 10, "4 queued, the other 10 dropped");
        // Positive control: nothing else reached the kernel while it's stuck.
        assert!(seen.try_recv().is_err());
    }

    fn tempdir() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("sin90-test-{}", crate::core::ulid()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }
}
