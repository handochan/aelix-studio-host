//! Loopback checks of the upgraded SDK backend. Production uses rmcp for the
//! team stdio server; the host connection probe has its own HTTP implementation.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::{Json, Router, body::Body, http::StatusCode, response::Response, routing::post};
use futures_util::{StreamExt, stream};
use rmcp::{ServiceExt, transport::StreamableHttpClientTransport};
use serde_json::{Value, json};

const JSON_LIMIT: usize = 16 * 1024 * 1024;
const ERROR_LIMIT: usize = 64 * 1024;

async fn listen(router: Router) -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/mcp", listener.local_addr().unwrap());
    let task = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    (url, task)
}

#[tokio::test]
async fn sdk_http_connects_lists_tools_and_preserves_rpc_errors() {
    let methods = Arc::new(Mutex::new(Vec::new()));
    let recorded = methods.clone();
    let router = Router::new().route(
        "/mcp",
        post(move |headers: axum::http::HeaderMap, Json(request): Json<Value>| {
            let recorded = recorded.clone();
            async move {
                let method = request["method"].as_str().unwrap();
                recorded.lock().unwrap().push(method.to_owned());
                if method == "notifications/initialized" {
                    assert_eq!(headers["mcp-session-id"], "host23-session");
                    return Response::builder().status(202).body(Body::empty()).unwrap();
                }
                let mut reply = json!({"jsonrpc": "2.0", "id": request["id"]});
                match method {
                    "initialize" => {
                        reply["result"] = json!({
                            "protocolVersion": "2025-11-25", "capabilities": {"tools": {}},
                            "serverInfo": {"name": "host23-fixture", "version": "1"}
                        })
                    }
                    "tools/list" => {
                        assert_eq!(headers["mcp-session-id"], "host23-session");
                        reply["result"] = json!({"tools": [{"name": "fixture_tool",
                            "description": "Fixture", "inputSchema": {"type": "object"}}]});
                    }
                    _ => reply["error"] = json!({"code": -32601, "message": "Method not found"}),
                }
                Response::builder()
                    .header("content-type", "application/json")
                    .header("mcp-session-id", "host23-session")
                    .body(Body::from(reply.to_string()))
                    .unwrap()
            }
        })
        .get(|| async { StatusCode::METHOD_NOT_ALLOWED })
        .delete(|| async { StatusCode::OK }),
    );
    let (url, server) = listen(router).await;
    let client = tokio::time::timeout(
        Duration::from_secs(10),
        ().serve(StreamableHttpClientTransport::from_uri(url)),
    )
    .await
    .unwrap()
    .expect("SDK HTTP initialization");
    let tools = client.list_tools(None).await.unwrap();
    assert_eq!(tools.tools.len(), 1);
    assert_eq!(tools.tools[0].name, "fixture_tool");
    let error = client
        .call_tool(rmcp::model::CallToolRequestParams::new("missing"))
        .await
        .unwrap_err();
    assert!(matches!(error, rmcp::ServiceError::McpError(ref data) if data.code.0 == -32601));
    client.cancel().await.unwrap();
    let methods = methods.lock().unwrap();
    for method in ["initialize", "notifications/initialized", "tools/list", "tools/call"] {
        assert!(
            methods.iter().any(|seen| seen == method),
            "missing {method}: {methods:?}"
        );
    }
    server.abort();
}

async fn oversized_response_is_rejected(status: StatusCode, limit: usize, chunked: bool) {
    let router = Router::new().route(
        "/mcp",
        post(move || async move {
            let bytes = vec![b' '; limit + 1];
            let body = if chunked {
                // Deliberately leave the response open: rejection must happen as
                // soon as the byte limit is crossed, without waiting for EOF.
                Body::from_stream(stream::once(async move { Ok::<_, std::io::Error>(bytes) }).chain(stream::pending()))
            } else {
                Body::from(bytes)
            };
            Response::builder()
                .status(status)
                .header("content-type", "application/json")
                .body(body)
                .unwrap()
        }),
    );
    let (url, server) = listen(router).await;
    let outcome = tokio::time::timeout(
        Duration::from_secs(10),
        ().serve(StreamableHttpClientTransport::from_uri(url)),
    )
    .await
    .expect("oversized response must fail without waiting for EOF");
    let error = match outcome {
        Ok(_) => panic!("oversized response accepted"),
        Err(error) => error,
    };
    assert!(
        error.to_string().contains(&format!(
            "HTTP response body exceeded the maximum size of {limit} bytes"
        )),
        "{error}"
    );
    server.abort();
}

#[tokio::test]
async fn sdk_default_limit_rejects_oversized_json_content_length() {
    oversized_response_is_rejected(StatusCode::OK, JSON_LIMIT, false).await;
}

#[tokio::test]
async fn sdk_default_limit_rejects_open_chunked_json() {
    oversized_response_is_rejected(StatusCode::OK, JSON_LIMIT, true).await;
}

#[tokio::test]
async fn sdk_default_limit_rejects_open_chunked_error_body() {
    oversized_response_is_rejected(StatusCode::BAD_REQUEST, ERROR_LIMIT, true).await;
}
