//! Bounded readers shared by the Host MCP probe and the patched session transports.

use bytes::{Buf, Bytes};
use reqwest::Response;

/// Match rmcp 3.9's default JSON and SSE event limits.
pub const MAX_RESPONSE_BYTES: usize = 16 * 1024 * 1024;
/// Match rmcp 3.9's default for error bodies that callers actually read.
pub const MAX_ERROR_BYTES: usize = 64 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ResponseReadError {
    #[error("MCP_RESPONSE_BODY_TOO_LARGE: HTTP response body exceeds its size limit")]
    BodyTooLarge,
    #[error("MCP_SSE_EVENT_TOO_LARGE: SSE event exceeds its size limit")]
    SseEventTooLarge,
    #[error("Failed to read MCP HTTP response")]
    ReadFailed,
}

struct LimitedBuffer {
    bytes: Vec<u8>,
    limit: usize,
}

impl LimitedBuffer {
    fn new(limit: usize) -> Self {
        Self {
            bytes: Vec::new(),
            limit,
        }
    }

    fn append(&mut self, chunk: &[u8], too_large: ResponseReadError) -> Result<(), ResponseReadError> {
        if chunk.len() > self.limit - self.bytes.len() {
            return Err(too_large);
        }

        let needed = self.bytes.len() + chunk.len();
        if needed > self.bytes.capacity() {
            let capacity = needed.max(self.bytes.capacity().saturating_mul(2)).min(self.limit);
            self.bytes.reserve_exact(capacity - self.bytes.len());
        }
        self.bytes.extend_from_slice(chunk);
        Ok(())
    }
}

/// Read a finite HTTP body with or without a declared length.
pub async fn read_body(mut response: Response, limit: usize) -> Result<Vec<u8>, ResponseReadError> {
    let too_large = ResponseReadError::BodyTooLarge;
    if response.content_length().is_some_and(|length| length > limit as u64) {
        return Err(too_large);
    }

    let mut body = LimitedBuffer::new(limit);
    while let Some(chunk) = response.chunk().await.map_err(|_| ResponseReadError::ReadFailed)? {
        body.append(&chunk, too_large)?;
    }
    Ok(body.bytes)
}

/// A persistent SSE connection retains at most one partial frame and one HTTP chunk.
/// The size limit includes line endings, comments and the terminating blank line.
pub struct SseReader {
    response: Response,
    chunk: Bytes,
    frame: LimitedBuffer,
    remaining_response: Option<usize>,
}

impl SseReader {
    pub fn new(response: Response) -> Self {
        Self {
            response,
            chunk: Bytes::new(),
            frame: LimitedBuffer::new(MAX_RESPONSE_BYTES),
            remaining_response: None,
        }
    }

    /// A POST awaits one RPC response. Bound its total input as well as each frame.
    pub fn for_rpc(response: Response) -> Self {
        let mut reader = Self::new(response);
        reader.remaining_response = Some(2 * MAX_RESPONSE_BYTES);
        reader
    }

    /// Read one frame, preserving any subsequent data from the same HTTP chunk.
    pub async fn next_frame(&mut self) -> Result<Option<&[u8]>, ResponseReadError> {
        self.frame.bytes.clear();
        let mut line_start = 0;

        loop {
            if self.chunk.is_empty() {
                let Some(chunk) = self.response.chunk().await.map_err(|_| ResponseReadError::ReadFailed)? else {
                    return Ok(None);
                };
                self.chunk = chunk;
            }

            let line_end = self.chunk.iter().position(|&byte| byte == b'\n');
            let length = line_end.map_or(self.chunk.len(), |end| end + 1);
            self.frame
                .append(&self.chunk[..length], ResponseReadError::SseEventTooLarge)?;
            if let Some(remaining) = &mut self.remaining_response {
                *remaining = remaining.checked_sub(length).ok_or(ResponseReadError::BodyTooLarge)?;
            }
            self.chunk.advance(length);

            if line_end.is_some() {
                let end = self.frame.bytes.len();
                let line = &self.frame.bytes[line_start..end - 1];
                if line.is_empty() || line == b"\r" {
                    return Ok(Some(&self.frame.bytes[..end]));
                }
                line_start = end;
            }
        }
    }

    /// A connection probe historically accepted a final data line without a blank line.
    pub fn unfinished_frame(&self) -> &[u8] {
        &self.frame.bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::StreamExt as _;

    #[test]
    fn buffer_never_reserves_for_overflow_and_accepts_exact_limit() {
        for error in [ResponseReadError::BodyTooLarge, ResponseReadError::SseEventTooLarge] {
            let mut buffer = LimitedBuffer::new(31);
            for _ in 0..31 {
                buffer.append(b"x", error).unwrap();
                assert!(buffer.bytes.capacity() <= 31);
            }
            assert_eq!(buffer.bytes.len(), 31);
            assert_eq!(buffer.append(b"x", error), Err(error));
            assert_eq!(buffer.bytes.len(), 31);
            assert_eq!(buffer.bytes.capacity(), 31);
            buffer.bytes.clear();
            assert_eq!(buffer.append(&[0; 32], error), Err(error));
            assert!(buffer.bytes.is_empty());
        }
    }

    fn streaming_response(chunks: impl futures_util::Stream<Item = Bytes> + Send + 'static) -> Response {
        http::Response::new(reqwest::Body::wrap_stream(chunks.map(Ok::<_, std::io::Error>))).into()
    }

    #[tokio::test]
    async fn reads_exact_chunked_body_and_rejects_overflow_without_waiting_for_eof() {
        use futures_util::stream;

        let response = streaming_response(stream::iter([Bytes::from_static(b"abc"), Bytes::from_static(b"def")]));
        assert_eq!(response.content_length(), None);
        assert_eq!(read_body(response, 6).await.unwrap(), b"abcdef");

        let response = streaming_response(
            stream::iter([Bytes::from_static(b"abc"), Bytes::from_static(b"de")]).chain(stream::pending()),
        );
        assert_eq!(
            read_body(response, 4).await.unwrap_err(),
            ResponseReadError::BodyTooLarge
        );
    }

    #[tokio::test]
    async fn rejects_declared_size_before_buffering() {
        let response = http::Response::new(reqwest::Body::from("abcde")).into();
        assert_eq!(
            read_body(response, 4).await.unwrap_err(),
            ResponseReadError::BodyTooLarge
        );
    }

    #[tokio::test]
    async fn accepts_exact_frame_with_split_crlf_and_keeps_following_frame() {
        let chunks = [
            Bytes::from_static(b"x\r"),
            Bytes::from_static(b"\n\r"),
            Bytes::from_static(b"\ny\n\n"),
        ];
        let mut reader = SseReader::new(streaming_response(futures_util::stream::iter(chunks)));
        reader.frame = LimitedBuffer::new(5);

        assert_eq!(reader.next_frame().await.unwrap().unwrap(), b"x\r\n\r\n");
        assert_eq!(reader.frame.bytes.capacity(), 5);
        assert_eq!(reader.next_frame().await.unwrap().unwrap(), b"y\n\n");
        assert!(reader.next_frame().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn drip_of_unterminated_frame_never_grows_buffer_past_limit() {
        let chunks = futures_util::stream::repeat_with(|| Bytes::from_static(b"x")).then(|byte| async {
            tokio::task::yield_now().await;
            byte
        });
        let mut reader = SseReader::new(streaming_response(chunks));
        reader.frame = LimitedBuffer::new(31);

        assert_eq!(
            reader.next_frame().await.unwrap_err(),
            ResponseReadError::SseEventTooLarge
        );
        assert_eq!(reader.frame.bytes.len(), 31);
        assert_eq!(reader.frame.bytes.capacity(), 31);
    }

    #[tokio::test]
    async fn finite_sse_response_stops_after_many_small_frames() {
        let chunks = futures_util::stream::repeat_with(|| Bytes::from_static(b": ping\n\n"));
        let mut reader = SseReader::for_rpc(streaming_response(chunks));
        reader.remaining_response = Some(2 * b": ping\n\n".len());

        assert!(reader.next_frame().await.unwrap().is_some());
        assert!(reader.next_frame().await.unwrap().is_some());
        assert_eq!(reader.next_frame().await.unwrap_err(), ResponseReadError::BodyTooLarge);
    }
}
