//! Test-only fake-kernel plumbing shared by the three typed client test
//! suites (`scheduler`/`memory`/`approval`) — one place to build a
//! [`crate::adapter_agent24::KernelClients`] over an in-memory socket pair
//! with an injected `Offer`, so each client's tests do not each reimplement
//! it. Mirrors `adapter_agent24::mod`'s own
//! `clients_over_a_socket_pair`/`noop_hook` test helpers, which this module
//! cannot reuse directly (`#[cfg(test)]` items in a `mod tests` block are not
//! `pub` to sibling modules) — the duplication is contained to this one file.

#![cfg(test)]

use std::sync::Arc;

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::UnixStream;

use crate::adapter_agent24::transport::Transport;
use crate::adapter_agent24::{FatalHook, KernelClients};

/// The fake kernel's end of the socket pair — a persistent reader (so
/// multiple `read_request` calls in one test never drop buffered bytes) plus
/// a plain write half.
pub(crate) struct FakePeer {
    reader: BufReader<OwnedReadHalf>,
    writer: OwnedWriteHalf,
}

fn noop_hook() -> FatalHook {
    Arc::new(|| {})
}

/// Builds a `KernelClients` with the given `Offer` injected directly (no real
/// handshake — `KernelClients::from_transport` skips straight past it, same
/// as `adapter_agent24::mod`'s own test helper) over one half of an in-memory
/// socket pair, and hands back the other half as a [`FakePeer`] a test drives
/// by hand.
pub(crate) async fn fake_kernel(offer: Vec<String>) -> (Arc<KernelClients>, FakePeer) {
    let (a, b) = std::os::unix::net::UnixStream::pair().unwrap();
    a.set_nonblocking(true).unwrap();
    b.set_nonblocking(true).unwrap();
    let stream = UnixStream::from_std(a).unwrap();
    let peer = UnixStream::from_std(b).unwrap();
    let transport = Transport::spawn(stream, noop_hook());
    let (read_half, writer) = peer.into_split();
    let peer = FakePeer {
        reader: BufReader::new(read_half),
        writer,
    };
    (
        Arc::new(KernelClients::from_transport(transport, offer)),
        peer,
    )
}

/// Reads one JSON-RPC request line off `peer` — whichever the client under
/// test just sent.
pub(crate) async fn read_request(peer: &mut FakePeer) -> Value {
    let mut buf = Vec::new();
    peer.reader.read_until(b'\n', &mut buf).await.unwrap();
    serde_json::from_slice(&buf).unwrap()
}

/// Answers `req` with a successful `result`.
pub(crate) async fn respond(peer: &mut FakePeer, req: &Value, result: Value) {
    let resp = json!({"jsonrpc": "2.0", "id": req["id"], "result": result});
    write_line(peer, &resp).await;
}

/// Answers `req` with an application error — `kind` empty means "no
/// `data.kind` at all" (e.g. a bare `-32602`), matching how a real
/// `-32602 invalid params` response has no `data` object at all.
pub(crate) async fn respond_error(
    peer: &mut FakePeer,
    req: &Value,
    code: i64,
    kind: &str,
    message: &str,
) {
    let error = if kind.is_empty() {
        json!({"code": code, "message": message})
    } else {
        json!({"code": code, "message": message, "data": {"kind": kind}})
    };
    let resp = json!({"jsonrpc": "2.0", "id": req["id"], "error": error});
    write_line(peer, &resp).await;
}

async fn write_line(peer: &mut FakePeer, value: &Value) {
    let mut bytes = serde_json::to_vec(value).unwrap();
    bytes.push(b'\n');
    peer.writer.write_all(&bytes).await.unwrap();
}
