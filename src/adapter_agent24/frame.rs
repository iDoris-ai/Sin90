//! NDJSON framing shared by the handshake (`connect_and_initialize`, run once
//! per connection) and the post-handshake multiplexed
//! [`crate::adapter_agent24::transport`] (run continuously by the writer/
//! reader tasks). One place that knows a frame is one `\n`-terminated JSON
//! value bounded at [`MAX_FRAME_BYTES`] — the same number Agent24 uses for
//! its own `agent24_os_proto::frame::MAX_FRAME_BYTES` (`docs/STATUS.md`).
//! Nothing enforces the two staying equal; if the kernel's bound ever
//! changes, this one has to be updated by hand — there is no shared crate
//! (yet) that would make them the same constant.
//!
//! # Boundary rule (mirrors `agent24-os-proto::frame::read_frame`, byte for
//! byte)
//!
//! A frame's CONTENT (the bytes before `\n`, not counting the newline
//! itself) may be at most [`MAX_FRAME_BYTES`] long — a content length of
//! exactly `MAX_FRAME_BYTES` is legal, `MAX_FRAME_BYTES + 1` is
//! [`FrameError::TooLong`]. The newline is popped and excluded from the
//! returned bytes only in the legal case; the length decision is made
//! against the content, after the newline would have been popped, never
//! against "however many bytes happened to include the delimiter or not." A
//! final line at EOF with no trailing `\n` is [`FrameError::Closed`] — a
//! partial line is not a frame, whether nothing was read at all or the
//! stream ended mid-line.
//!
//! This function distinguishes those two `Closed` sub-cases (nothing read;
//! partial line at EOF) from a genuine over-long line only by comparing how
//! many bytes were actually consumed against the read budget — see
//! [`read_frame`]'s body for why that comparison, not the trailing byte
//! alone, is what decides `TooLong` vs `Closed`.

use serde_json::Value;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt};

pub const MAX_FRAME_BYTES: usize = 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum FrameError {
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("serialization: {0}")]
    Serde(#[from] serde_json::Error),
    #[error("frame content exceeds {MAX_FRAME_BYTES} bytes")]
    TooLong,
    /// The peer closed the connection cleanly, mid-frame or before one
    /// started — including a final line with no trailing `\n` at EOF, which
    /// is a partial line, not a frame.
    #[error("the connection closed before a full frame arrived")]
    Closed,
}

/// Serialize `value` as one NDJSON line and write it whole. Callers must not
/// interleave two `write_frame` calls on the same writer without external
/// synchronization (the multiplexed transport gives every writer a single
/// dedicated task for exactly this reason — see `transport::Transport`).
pub async fn write_frame<W: AsyncWrite + Unpin>(
    w: &mut W,
    value: &Value,
) -> Result<(), FrameError> {
    let mut bytes = serde_json::to_vec(value)?;
    bytes.push(b'\n');
    w.write_all(&bytes).await?;
    w.flush().await?;
    Ok(())
}

/// Read one NDJSON line: everything up to the next `\n`, with the newline
/// consumed and not returned. See the module docs for the exact boundary
/// (content of exactly `MAX_FRAME_BYTES` is legal; `MAX_FRAME_BYTES + 1` is
/// `TooLong`; a partial final line at EOF is `Closed`, not a frame).
///
/// Implementation note: a `Take` adapter caps how many bytes `read_until` may
/// pull from `r` at `MAX_FRAME_BYTES + 1` — enough to see one byte past the
/// limit, which is what makes "exactly at the limit" decidable without
/// reading further. Three outcomes fall out of that:
/// - the delimiter was found within the cap → legal frame, newline popped;
/// - the cap was reached (`buf.len() == MAX_FRAME_BYTES + 1`) without a
///   delimiter → `TooLong` (real content already exceeds the limit);
/// - fewer bytes than the cap were read and still no delimiter → the
///   underlying reader genuinely ran out (real EOF), not the artificial cap
///   → `Closed`.
pub async fn read_frame<R: AsyncBufRead + Unpin>(r: &mut R) -> Result<Vec<u8>, FrameError> {
    let take_limit = (MAX_FRAME_BYTES + 1) as u64;
    let mut buf = Vec::new();
    let mut limited = tokio::io::AsyncReadExt::take(r, take_limit);
    let n = limited.read_until(b'\n', &mut buf).await?;
    if n == 0 {
        return Err(FrameError::Closed);
    }
    if buf.last() == Some(&b'\n') {
        buf.pop();
        return Ok(buf);
    }
    if buf.len() as u64 >= take_limit {
        return Err(FrameError::TooLong);
    }
    Err(FrameError::Closed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[tokio::test]
    async fn read_frame_content_of_exactly_max_bytes_is_legal() {
        let content = vec![b'a'; MAX_FRAME_BYTES];
        let mut input = content.clone();
        input.push(b'\n');
        let mut cursor = Cursor::new(input);
        let frame = read_frame(&mut cursor).await.unwrap();
        assert_eq!(frame, content);
    }

    #[tokio::test]
    async fn read_frame_content_of_max_plus_one_bytes_is_too_long() {
        let content = vec![b'a'; MAX_FRAME_BYTES + 1];
        let mut input = content;
        input.push(b'\n');
        let mut cursor = Cursor::new(input);
        let err = read_frame(&mut cursor).await.unwrap_err();
        assert!(matches!(err, FrameError::TooLong), "got {err:?}");
    }

    #[tokio::test]
    async fn read_frame_partial_final_line_at_eof_is_closed_not_a_frame() {
        // No trailing `\n` at all — the stream ends mid-line.
        let mut cursor = Cursor::new(b"incomplete".to_vec());
        let err = read_frame(&mut cursor).await.unwrap_err();
        assert!(matches!(err, FrameError::Closed), "got {err:?}");
    }

    #[tokio::test]
    async fn read_frame_on_a_completely_empty_stream_is_closed() {
        let mut cursor = Cursor::new(Vec::new());
        let err = read_frame(&mut cursor).await.unwrap_err();
        assert!(matches!(err, FrameError::Closed), "got {err:?}");
    }

    #[tokio::test]
    async fn read_frame_pops_the_newline_and_leaves_the_next_frame_intact() {
        let mut cursor = Cursor::new(b"{\"a\":1}\n{\"b\":2}\n".to_vec());
        let first = read_frame(&mut cursor).await.unwrap();
        assert_eq!(first, b"{\"a\":1}");
        let second = read_frame(&mut cursor).await.unwrap();
        assert_eq!(second, b"{\"b\":2}");
    }
}
