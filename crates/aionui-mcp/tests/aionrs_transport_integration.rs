//! Integration coverage for runtime MCP handshake, tool discovery, and calls.

use std::collections::HashMap;

use aion_mcp::config::{McpServerConfig, TransportType};
use aion_mcp::manager::McpManager;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

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
                    r#"{"jsonrpc":"2.0","id":0,"result":{"content":[{"type":"text","text":"fixture result"}]}}"#,
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
