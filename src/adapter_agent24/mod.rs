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
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::Mutex;

use crate::http::EventSink;

/// NDJSON frame bound (design/`docs/STATUS.md`: matches Agent24's
/// `agent24_os_proto::frame::MAX_FRAME_BYTES`).
const MAX_FRAME_BYTES: usize = 1024 * 1024;

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
    #[error("callback frame exceeds {MAX_FRAME_BYTES} bytes")]
    FrameTooLong,
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
            match connect_and_initialize(
                &self.sock_path,
                &self.module,
                &self.manifest_bytes,
                &self.auth_token,
            )
            .await
            {
                Ok((reader, _provides)) => *conn = Some(reader),
                Err(e) => {
                    tracing::warn!(error = %e, kind, "sin90: could not reconnect to emit events/emit");
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
        // to read a desynced stream.
        let reader = conn.as_mut().expect("just ensured Some above");
        if let Err(e) = write_frame(reader.get_mut(), &req).await {
            tracing::warn!(error = %e, kind, "sin90: failed to write events/emit frame");
            *conn = None;
            return;
        }
        match read_frame(reader).await {
            Ok(line) => match serde_json::from_slice::<Value>(&line) {
                Ok(resp) => {
                    if let Some(err) = resp.get("error") {
                        tracing::warn!(kind, ?err, "sin90: events/emit rejected by kernel");
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, kind, "sin90: events/emit response was not valid JSON");
                    *conn = None;
                }
            },
            Err(e) => {
                tracing::warn!(error = %e, kind, "sin90: no response to events/emit");
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
    write_frame(reader.get_mut(), &req).await?;

    let line = read_frame(&mut reader).await?;
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
pub struct KernelEventSink(pub Arc<CallbackChannel>);
impl EventSink for KernelEventSink {
    fn emit(&self, kind: &str, payload: Map<String, Value>) {
        let chan = self.0.clone();
        let kind = kind.to_string();
        // Fire-and-forget: `EventSink::emit` is a sync trait method called from
        // inside an async handler that must not block on the callback round
        // trip for the HTTP response itself (design §5.3's "degrade, don't
        // fail" — the caller's write already committed).
        tokio::spawn(async move { chan.emit(&kind, payload).await });
    }
}

async fn write_frame(stream: &mut UnixStream, value: &Value) -> Result<(), AdapterError> {
    let mut bytes = serde_json::to_vec(value)?;
    bytes.push(b'\n');
    stream.write_all(&bytes).await?;
    stream.flush().await?;
    Ok(())
}

/// Read one NDJSON line, bounded (mirrors Agent24's `frame::read_frame`
/// contract: refuse an over-long line rather than buffer it unbounded).
async fn read_frame(reader: &mut BufReader<UnixStream>) -> Result<Vec<u8>, AdapterError> {
    let mut buf = Vec::new();
    let mut limited = tokio::io::AsyncReadExt::take(reader, (MAX_FRAME_BYTES + 1) as u64);
    let n = limited.read_until(b'\n', &mut buf).await?;
    if n == 0 {
        return Err(AdapterError::Io(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "callback socket closed",
        )));
    }
    if buf.len() > MAX_FRAME_BYTES {
        return Err(AdapterError::FrameTooLong);
    }
    if buf.last() == Some(&b'\n') {
        buf.pop();
    }
    Ok(buf)
}

/// Build a `tokio::net::TcpListener` from the kernel-bound `A24_LISTEN_FD`.
///
/// # Safety
/// The fd is a real, kernel-opened, non-owned-by-anything-else listener
/// socket for the lifetime of this process — that is the entire contract
/// `A24_LISTEN_FD` exists to state (`docs/STATUS.md`). This function must be
/// called at most once per process.
pub fn listener_from_fd(fd: i32) -> std::io::Result<tokio::net::TcpListener> {
    let std_listener = unsafe { std::net::TcpListener::from_raw_fd(fd) };
    std_listener.set_nonblocking(true)?;
    tokio::net::TcpListener::from_std(std_listener)
}

#[cfg(test)]
mod tests {
    use super::*;

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

    fn tempdir() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("sin90-test-{}", crate::core::ulid()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }
}
