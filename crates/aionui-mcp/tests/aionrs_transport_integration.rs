//! Integration coverage for runtime MCP handshake, tool discovery, and calls.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use aion_mcp::config::{McpServerConfig, TransportType};
use aion_mcp::manager::McpManager;
use aion_mcp::protocol::JsonRpcRequest;
use aion_mcp::transport::{McpError, McpTransport, sse::SseTransport, streamable_http::StreamableHttpTransport};
use aionui_api_types::McpConnectionTestErrorCode;
use aionui_mcp::{McpConnectionTestService, McpServerTransport};
use aionui_mcp_http::{MAX_RESPONSE_BYTES, MAX_SSE_RESPONSE_BYTES, ResponseReadError};
use aionui_realtime::BroadcastEventBus;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// Build an HTTP response containing one CRLF-delimited SSE event.
fn sse_response(json_rpc: &str, session_id: Option<&str>) -> Vec<u8> {
    let body = format!("event: message\r\ndata: {json_rpc}\r\n\r\n");
    let mut head = String::from("HTTP/1.1 200 OK\r\n");
    head.push_str("Content-Type: text/event-stream\r\n");
    if let Some(sid) = session_id {
        head.push_str(&format!("mcp-session-id: {sid}\r\n"));
    }
    head.push_str(&format!("Content-Length: {}\r\n", body.len()));
    head.push_str("Connection: close\r\n\r\n");
    head.push_str(&body);
    head.into_bytes()
}

fn accepted_204() -> Vec<u8> {
    b"HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec()
}

/// Read one full HTTP request (headers + Content-Length body) from the socket.
async fn read_http_request(socket: &mut tokio::net::TcpStream) -> String {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 1024];
    // Read headers.
    let header_end = loop {
        let n = socket.read(&mut tmp).await.unwrap();
        if n == 0 {
            return String::from_utf8_lossy(&buf).into_owned();
        }
        buf.extend_from_slice(&tmp[..n]);
        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos + 4;
        }
    };
    let headers = String::from_utf8_lossy(&buf[..header_end]).to_lowercase();
    let content_length = headers
        .lines()
        .find_map(|l| l.strip_prefix("content-length:"))
        .and_then(|v| v.trim().parse::<usize>().ok())
        .unwrap_or(0);
    while buf.len() < header_end + content_length {
        let n = socket.read(&mut tmp).await.unwrap();
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&tmp[..n]);
    }
    String::from_utf8_lossy(&buf).into_owned()
}

fn rpc_id(request: &str) -> u64 {
    let (_, body) = request.split_once("\r\n\r\n").unwrap();
    serde_json::from_str::<serde_json::Value>(body).unwrap()["id"]
        .as_u64()
        .unwrap()
}

#[tokio::test]
async fn stateful_fastmcp_style_server_exposes_tools() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let url = format!("http://{addr}/mcp");

    // Faithful stateful server: returns Mcp-Session-Id on initialize and serves
    // every JSON-RPC response as CRLF-delimited SSE.
    tokio::spawn(async move {
        loop {
            let (mut socket, _) = listener.accept().await.unwrap();
            let request = read_http_request(&mut socket).await;

            let response = if request.contains("\"initialize\"") {
                sse_response(
                    r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-03-26","capabilities":{"tools":{}},"serverInfo":{"name":"fastmcp-test","version":"1.0"}}}"#,
                    Some("session-abc-123"),
                )
            } else if request.contains("\"tools/list\"") {
                sse_response(
                    r#"{"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"meta_ads_query","description":"Query Meta ads","inputSchema":{"type":"object"}}]}}"#,
                    Some("session-abc-123"),
                )
            } else if request.contains("\"tools/call\"") {
                sse_response(
                    &serde_json::json!({
                        "jsonrpc": "2.0", "id": rpc_id(&request),
                        "result": {"content": [{"type":"text", "text":"fixture result"}]}
                    })
                    .to_string(),
                    Some("session-abc-123"),
                )
            } else {
                // notifications/initialized and anything else
                accepted_204()
            };

            socket.write_all(&response).await.unwrap();
            let _ = socket.flush().await;
        }
    });

    let config = McpServerConfig {
        transport: TransportType::StreamableHttp,
        command: None,
        args: None,
        env: None,
        url: Some(url),
        headers: None,
        deferred: None,
        startup_timeout_ms: Some(5_000),
    };
    let configs = HashMap::from([("meta-ads".to_string(), config)]);

    let manager = McpManager::connect_all(&configs).await.unwrap();

    assert!(
        manager.has_tool_name("meta_ads_query"),
        "stateful CRLF-SSE server tools must be discovered and exposed to the model"
    );
    assert_eq!(manager.tool_name_count("meta_ads_query"), 1);
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        manager.call_tool("meta-ads", "meta_ads_query", serde_json::json!({})),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(result, "fixture result");
}

#[tokio::test]
async fn legacy_sse_connects_lists_and_calls_tools() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let peer = tokio::spawn(async move {
        let (mut events, _) = listener.accept().await.unwrap();
        assert!(read_http_request(&mut events).await.starts_with("GET /sse "));
        events
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\r\nevent: endpoint\r\ndata: /messages\r\n\r\n",
            )
            .await
            .unwrap();

        for (method, reply) in [
            (
                "initialize",
                r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-03-26","capabilities":{"tools":{}},"serverInfo":{"name":"fixture","version":"1.0"}}}"#,
            ),
            ("notifications/initialized", ""),
            (
                "tools/list",
                r#"{"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"query","description":"Fixture","inputSchema":{"type":"object"}}]}}"#,
            ),
            (
                "tools/call",
                r#"{"jsonrpc":"2.0","id":0,"result":{"content":[{"type":"text","text":"fixture result"}]}}"#,
            ),
        ] {
            let (mut post, _) = listener.accept().await.unwrap();
            let request = read_http_request(&mut post).await;
            assert!(request.starts_with("POST /messages "));
            assert!(request.contains(&format!("\"{method}\"")));
            post.write_all(&accepted_204()).await.unwrap();
            if !reply.is_empty() {
                let mut reply: serde_json::Value = serde_json::from_str(reply).unwrap();
                reply["id"] = rpc_id(&request).into();
                events
                    .write_all(format!("event: message\r\ndata: {reply}\r\n\r\n").as_bytes())
                    .await
                    .unwrap();
            }
        }
    });

    let config = McpServerConfig {
        transport: TransportType::Sse,
        command: None,
        args: None,
        env: None,
        url: Some(format!("http://{addr}/sse")),
        headers: None,
        deferred: None,
        startup_timeout_ms: Some(5_000),
    };

    let manager = McpManager::connect_all(&HashMap::from([("legacy".into(), config)]))
        .await
        .unwrap();
    assert!(manager.has_tool_name("query"));
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        manager.call_tool("legacy", "query", serde_json::json!({})),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(result, "fixture result");
    peer.await.unwrap();
}

const PEER_DEADLINE: Duration = Duration::from_secs(10);
const SSE_HEADERS: &[u8] = b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n";
const ENDPOINT: &[u8] = b"event: endpoint\ndata: /messages\n\n";
const INIT_REPLY: &[u8] = b"event: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}\n\n";
const LIST_REPLY: &[u8] = b"event: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{\"tools\":[]}}\n\n";
const MALFORMED_REPLY: &[u8] = b"event: message\ndata: {broken\n\n";

async fn accept_stream(peer: &TcpListener) -> TcpStream {
    let (mut stream, _) = peer.accept().await.unwrap();
    assert!(read_http_request(&mut stream).await.starts_with("GET /sse "));
    stream.write_all(SSE_HEADERS).await.unwrap();
    stream
}

async fn accept_rpc(peer: &TcpListener, method: &str) -> TcpStream {
    let (mut post, _) = peer.accept().await.unwrap();
    let request = read_http_request(&mut post).await;
    assert!(request.starts_with("POST /messages "));
    assert!(request.contains(&format!("\"{method}\"")));
    post
}

async fn write_repeated(stream: &mut TcpStream, block: &[u8], bytes: usize) -> std::io::Result<()> {
    for _ in 0..bytes.div_ceil(block.len()) {
        stream.write_all(block).await?;
    }
    Ok(())
}

async fn write_keepalives(stream: &mut TcpStream, bytes: usize) -> std::io::Result<()> {
    let mut frame = [b'x'; 4096];
    frame[0] = b':';
    frame[4094..].copy_from_slice(b"\n\n");
    write_repeated(stream, &frame, bytes).await
}

fn probe_service() -> McpConnectionTestService {
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    McpConnectionTestService::new(client, Arc::new(BroadcastEventBus::new(4)))
}

async fn run_probe(transport: McpServerTransport) -> aionui_api_types::McpConnectionTestResult {
    probe_service().test_connection("probe", &transport).await
}

async fn probe_sse(url: String) -> aionui_api_types::McpConnectionTestResult {
    run_probe(McpServerTransport::Sse {
        url,
        headers: HashMap::new(),
    })
    .await
}

fn assert_runtime_limit<T>(result: Result<T, McpError>, expected: ResponseReadError) {
    match result {
        Err(McpError::ResponseRead(error)) => assert_eq!(error, expected),
        Err(error) => panic!("unexpected runtime error: {error}"),
        Ok(_) => panic!("runtime accepted an oversized response"),
    };
}

#[tokio::test]
async fn streamable_http_body_fails_probe_and_runtime_before_eof() {
    for probe in [true, false] {
        let peer = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/mcp", peer.local_addr().unwrap());
        let writer = tokio::spawn(async move {
            let (mut stream, _) = peer.accept().await.unwrap();
            assert!(read_http_request(&mut stream).await.contains("\"initialize\""));
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n")
                .await
                .unwrap();
            let _ = write_repeated(&mut stream, &[b' '; 4096], MAX_RESPONSE_BYTES + 1).await;
            std::future::pending::<()>().await;
        });

        tokio::time::timeout(PEER_DEADLINE, async {
            if probe {
                let result = run_probe(McpServerTransport::Http {
                    url,
                    headers: HashMap::new(),
                })
                .await;
                assert_eq!(result.code, Some(McpConnectionTestErrorCode::ResponseBodyTooLarge));
            } else {
                let transport = StreamableHttpTransport::connect(&url, &HashMap::new()).await.unwrap();
                let result = transport.request(&JsonRpcRequest::new(1, "initialize", None)).await;
                assert_runtime_limit(result, ResponseReadError::BodyTooLarge);
            }
        })
        .await
        .expect("JSON body must fail on bytes, before EOF");
        assert!(!writer.is_finished());
        writer.abort();
    }
}

#[tokio::test]
async fn persistent_sse_endpoint_budget_applies_to_probe_and_runtime() {
    for probe in [true, false] {
        let peer = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/sse", peer.local_addr().unwrap());
        let writer = tokio::spawn(async move {
            let mut stream = accept_stream(&peer).await;
            let _ = write_keepalives(&mut stream, MAX_SSE_RESPONSE_BYTES + 1).await;
            // Keep the length-less body open until the client returns its error.
            std::future::pending::<()>().await;
        });

        tokio::time::timeout(PEER_DEADLINE, async {
            if probe {
                let result = probe_sse(url).await;
                assert_eq!(result.code, Some(McpConnectionTestErrorCode::ResponseBodyTooLarge));
            } else {
                let result = SseTransport::connect(&url, &HashMap::new()).await;
                assert_runtime_limit(result, ResponseReadError::BodyTooLarge);
            }
        })
        .await
        .expect("endpoint must fail on bytes, before stream EOF");
        assert!(!writer.is_finished());
        writer.abort();
    }
}

#[tokio::test]
async fn persistent_sse_budget_fails_waiting_probe_and_runtime_before_post_ack() {
    assert_sse_limit_before_post_ack(
        ResponseReadError::BodyTooLarge,
        McpConnectionTestErrorCode::ResponseBodyTooLarge,
    )
    .await;
}

#[tokio::test]
async fn persistent_sse_frame_limit_fails_waiting_probe_and_runtime_before_post_ack() {
    assert_sse_limit_before_post_ack(
        ResponseReadError::SseEventTooLarge,
        McpConnectionTestErrorCode::SseEventTooLarge,
    )
    .await;
}

async fn assert_sse_limit_before_post_ack(expected: ResponseReadError, probe_code: McpConnectionTestErrorCode) {
    for probe in [true, false] {
        let peer = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/sse", peer.local_addr().unwrap());
        let writer = tokio::spawn(async move {
            let mut stream = accept_stream(&peer).await;
            stream.write_all(ENDPOINT).await.unwrap();
            let _post = accept_rpc(&peer, "initialize").await;
            // This response is for another ID. The companion POST remains unacknowledged.
            stream
                .write_all(b"event: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":99,\"result\":{}}\n\n")
                .await
                .unwrap();
            if expected == ResponseReadError::BodyTooLarge {
                let _ = write_keepalives(&mut stream, MAX_SSE_RESPONSE_BYTES + 1).await;
            } else {
                stream.write_all(b"event: message\ndata: ").await.unwrap();
                let _ = write_repeated(&mut stream, &[b'x'; 4096], MAX_RESPONSE_BYTES + 1).await;
            }
            std::future::pending::<()>().await;
        });

        tokio::time::timeout(PEER_DEADLINE, async {
            if probe {
                let result = probe_sse(url).await;
                assert_eq!(result.code, Some(probe_code));
            } else {
                let transport = SseTransport::connect(&url, &HashMap::new()).await.unwrap();
                let result = transport.request(&JsonRpcRequest::new(1, "initialize", None)).await;
                assert_runtime_limit(result, expected);
                assert_runtime_limit(
                    transport.request(&JsonRpcRequest::new(2, "tools/list", None)).await,
                    expected,
                );
            }
        })
        .await
        .expect("RPC must fail on bytes, before POST ACK or stream EOF");
        assert!(!writer.is_finished());
        writer.abort();
    }
}

#[tokio::test]
async fn persistent_sse_probe_reports_malformed_rpc_reply_before_eof() {
    let peer = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let transport = McpServerTransport::Sse {
        url: format!("http://{}/sse", peer.local_addr().unwrap()),
        headers: HashMap::new(),
    };
    let writer = tokio::spawn(async move {
        let mut stream = accept_stream(&peer).await;
        stream.write_all(ENDPOINT).await.unwrap();
        let mut post = accept_rpc(&peer, "initialize").await;
        post.write_all(&accepted_204()).await.unwrap();
        stream
            .write_all(
                b": keepalive\n\nevent: message\ndata: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/message\"}\n\n",
            )
            .await
            .unwrap();
        stream
            .write_all(b"event: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":99,\"result\":{}}\n\n")
            .await
            .unwrap();
        stream.write_all(MALFORMED_REPLY).await.unwrap();
        std::future::pending::<()>().await;
    });

    let service = probe_service().with_timeout(Duration::from_secs(2));
    let result = tokio::time::timeout(PEER_DEADLINE, service.test_connection("probe", &transport))
        .await
        .expect("the probe must complete before the stream ends");
    assert_eq!(result.code, Some(McpConnectionTestErrorCode::ProtocolError));
    let error = result.error.unwrap();
    assert!(
        error.contains("Invalid JSON-RPC in SSE: key must be a string"),
        "{error}"
    );
    assert!(!writer.is_finished());
    writer.abort();
}

#[tokio::test]
async fn persistent_sse_passes_idle_keepalive_between_rpc_windows() {
    for probe in [true, false] {
        let peer = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/sse", peer.local_addr().unwrap());
        let writer = tokio::spawn(async move {
            let mut stream = accept_stream(&peer).await;
            stream.write_all(ENDPOINT).await.unwrap();

            let mut init = accept_rpc(&peer, "initialize").await;
            init.write_all(&accepted_204()).await.unwrap();
            write_keepalives(&mut stream, MAX_SSE_RESPONSE_BYTES / 2 + 4096)
                .await
                .unwrap();
            stream.write_all(INIT_REPLY).await.unwrap();

            let mut notify = accept_rpc(&peer, "notifications/initialized").await;
            // The notification has no outstanding RPC. Send more than one window's quota.
            stream.write_all(MALFORMED_REPLY).await.unwrap();
            write_keepalives(&mut stream, MAX_SSE_RESPONSE_BYTES + 8 * 1024 * 1024)
                .await
                .unwrap();
            notify.write_all(&accepted_204()).await.unwrap();

            let mut list = accept_rpc(&peer, "tools/list").await;
            list.write_all(&accepted_204()).await.unwrap();
            write_keepalives(&mut stream, MAX_SSE_RESPONSE_BYTES / 2 + 4096)
                .await
                .unwrap();
            stream.write_all(LIST_REPLY).await.unwrap();
        });

        tokio::time::timeout(PEER_DEADLINE, async {
            if probe {
                let result = probe_sse(url).await;
                assert!(result.success, "{:?}", result.error);
            } else {
                let transport = SseTransport::connect(&url, &HashMap::new()).await.unwrap();
                transport
                    .request(&JsonRpcRequest::new(1, "initialize", None))
                    .await
                    .unwrap();
                transport
                    .notify(&JsonRpcRequest::notification("notifications/initialized", None))
                    .await
                    .unwrap();
                let reply = transport
                    .request(&JsonRpcRequest::new(2, "tools/list", None))
                    .await
                    .unwrap();
                assert_eq!(reply.id, Some(2));
            }
            writer.await.unwrap();
        })
        .await
        .expect("the second RPC must complete after idle keepalive");
    }
}
