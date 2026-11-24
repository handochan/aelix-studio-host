use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

use aionui_mcp_http::{ResponseReadError, SseReader, SseRequestError, SseResponses};
use async_trait::async_trait;
use reqwest::header::{HeaderMap, HeaderValue};

use super::{McpError, McpTransport};
use crate::protocol::{JsonRpcRequest, JsonRpcResponse};

/// SSE transport: connects to an SSE endpoint for server→client events,
/// sends requests via POST to the endpoint URL received from the SSE stream
pub struct SseTransport {
    client: reqwest::Client,
    /// The POST endpoint URL (received from the SSE stream's "endpoint" event)
    post_url: String,
    headers: HeaderMap,
    responses: SseResponses<JsonRpcResponse>,
    next_id: AtomicU64,
    /// Handle to the background SSE listener task
    _listener: tokio::task::JoinHandle<()>,
}

impl Drop for SseTransport {
    fn drop(&mut self) {
        self._listener.abort();
    }
}

impl SseTransport {
    /// Connect to an SSE MCP server
    pub async fn connect(url: &str, headers: &HashMap<String, String>) -> Result<Self, McpError> {
        let mut header_map = HeaderMap::new();
        for (k, v) in headers {
            let name = reqwest::header::HeaderName::from_bytes(k.as_bytes())
                .map_err(|e| McpError::Transport(format!("Invalid header name '{}': {}", k, e)))?;
            let value = HeaderValue::from_str(v)
                .map_err(|e| McpError::Transport(format!("Invalid header value '{}': {}", v, e)))?;
            header_map.insert(name, value);
        }

        let client = reqwest::Client::new();

        // GET the SSE endpoint to establish the event stream
        let response = client
            .get(url)
            .headers(header_map.clone())
            .header("Accept", "text/event-stream")
            .send()
            .await
            .map_err(|e| McpError::Transport(format!("SSE connection failed: {}", e)))?;

        if !response.status().is_success() {
            return Err(McpError::Transport(format!(
                "SSE connection returned status: {}",
                response.status()
            )));
        }

        let responses = SseResponses::default();

        // The server sends an "endpoint" event with the POST URL.
        let base_url = extract_base_url(url);
        let mut reader = SseReader::for_rpc(response);
        let post_url = loop {
            let frame = reader
                .next_frame()
                .await?
                .ok_or_else(|| McpError::Transport("No endpoint event received from SSE".into()))?;
            let (event_type, event_data) = parse_sse_event(&String::from_utf8_lossy(frame));
            if event_type == "endpoint" {
                break if event_data.starts_with("http") {
                    event_data
                } else {
                    format!("{base_url}{event_data}")
                };
            }
        };

        // Continue with any buffered events following the endpoint in the same chunk.
        reader.finish_response();
        let listener_responses = responses.clone();
        let listener = tokio::spawn(async move {
            let error = loop {
                let frame = match reader.next_frame_with(|bytes| listener_responses.charge(bytes)).await {
                    Ok(Some(frame)) => frame,
                    Ok(None) => break ResponseReadError::ReadFailed,
                    Err(error) => break error,
                };
                let (event_type, event_data) = parse_sse_event(&String::from_utf8_lossy(frame));
                if (event_type == "message" || event_type.is_empty())
                    && let Ok(response) = serde_json::from_str::<JsonRpcResponse>(&event_data)
                    && let Some(id) = response.id
                {
                    listener_responses.deliver(id, response);
                }
            };
            listener_responses.close(error);
        });

        Ok(Self {
            client,
            post_url,
            headers: header_map,
            responses,
            next_id: AtomicU64::new(1),
            _listener: listener,
        })
    }

    pub fn next_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }
}

#[async_trait]
impl McpTransport for SseTransport {
    async fn request(&self, req: &JsonRpcRequest) -> Result<JsonRpcResponse, McpError> {
        let req_id = req
            .id
            .ok_or_else(|| McpError::Transport("Request must have an id".into()))?;

        let body = serde_json::to_string(req).map_err(|e| McpError::Transport(format!("JSON serialize error: {e}")))?;
        let request = self
            .client
            .post(&self.post_url)
            .headers(self.headers.clone())
            .header("Content-Type", "application/json")
            .body(body);
        let mut waiting = self.responses.start(req_id).map_err(|error| match error {
            SseRequestError::Closed(error) => McpError::from(error),
            SseRequestError::DuplicateId => McpError::Transport(error.to_string()),
        })?;

        // A receive-side failure can win before the companion POST is acknowledged.
        let post = async {
            let response = request
                .send()
                .await
                .map_err(|e| McpError::Transport(format!("POST request failed: {e}")))?;
            if !response.status().is_success() {
                return Err(McpError::Transport(format!(
                    "POST returned status: {}",
                    response.status()
                )));
            }
            Ok(())
        };
        let (rpc_response, ()) = tokio::try_join!(
            biased;
            async { waiting.receive().await.map_err(McpError::from) },
            post,
        )?;

        if let Some(err) = &rpc_response.error {
            return Err(McpError::JsonRpc {
                code: err.code,
                message: err.message.clone(),
            });
        }

        Ok(rpc_response)
    }

    async fn notify(&self, req: &JsonRpcRequest) -> Result<(), McpError> {
        let body =
            serde_json::to_string(req).map_err(|e| McpError::Transport(format!("JSON serialize error: {}", e)))?;

        self.client
            .post(&self.post_url)
            .headers(self.headers.clone())
            .header("Content-Type", "application/json")
            .body(body)
            .send()
            .await
            .map_err(|e| McpError::Transport(format!("Notification POST failed: {}", e)))?;

        Ok(())
    }

    async fn close(&self) -> Result<(), McpError> {
        self._listener.abort();
        self.responses.close(ResponseReadError::ReadFailed);
        Ok(())
    }
}

/// Parse a single SSE event block into (event_type, data)
fn parse_sse_event(block: &str) -> (String, String) {
    let mut event_type = String::new();
    let mut data_lines = Vec::new();

    for line in block.lines() {
        if let Some(value) = line.strip_prefix("event:") {
            event_type = value.trim().to_string();
        } else if let Some(value) = line.strip_prefix("data:") {
            data_lines.push(value.trim().to_string());
        }
    }

    (event_type, data_lines.join("\n"))
}

/// Extract base URL (scheme + host + port) from a full URL
fn extract_base_url(url: &str) -> String {
    // Find the position after "://"
    if let Some(scheme_end) = url.find("://") {
        let rest = &url[scheme_end + 3..];
        if let Some(path_start) = rest.find('/') {
            return url[..scheme_end + 3 + path_start].to_string();
        }
    }
    url.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn pending_post_does_not_hide_sse_limit_error() {
        let peer = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let transport = SseTransport {
            client: reqwest::Client::builder().no_proxy().build().unwrap(),
            post_url: format!("http://{}/messages", peer.local_addr().unwrap()),
            headers: HeaderMap::new(),
            responses: SseResponses::default(),
            next_id: AtomicU64::new(1),
            _listener: tokio::spawn(std::future::pending()),
        };
        let rpc = JsonRpcRequest::new(1, "tools/list", None);
        let call = transport.request(&rpc);
        tokio::pin!(call);
        let (_socket, _) = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            tokio::select! {
                result = &mut call => panic!("request completed before its response: {result:?}"),
                socket = peer.accept() => socket.unwrap(),
            }
        })
        .await
        .unwrap();

        transport.responses.close(ResponseReadError::SseEventTooLarge);
        assert!(matches!(
            tokio::time::timeout(std::time::Duration::from_secs(5), call)
                .await
                .unwrap()
                .unwrap_err(),
            McpError::ResponseRead(ResponseReadError::SseEventTooLarge)
        ));
    }
}
