//! Exercise the SDK's actual newline-delimited transport and tool macro router,
//! including the TCP translation and sanitized remote errors.
use super::*;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream};
use tokio::net::TcpListener;

async fn send(wire: &mut BufReader<DuplexStream>, message: Value) {
    wire.get_mut()
        .write_all(format!("{message}\n").as_bytes())
        .await
        .unwrap();
}

async fn reply(wire: &mut BufReader<DuplexStream>) -> Value {
    let mut line = String::new();
    tokio::time::timeout(std::time::Duration::from_secs(5), wire.read_line(&mut line))
        .await
        .unwrap()
        .unwrap();
    serde_json::from_str(&line).unwrap()
}

#[tokio::test]
async fn stdio_handshake_filtered_list_and_tool_results_survive_sdk_upgrade() {
    let listener = TcpListener::bind((CONNECT_HOST, 0)).await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let tcp = tokio::spawn(async move {
        for round in 0..3 {
            let (mut socket, _) = listener.accept().await.unwrap();
            let init: Value = serde_json::from_slice(&read_frame(&mut socket).await.unwrap()).unwrap();
            assert_eq!(init["params"]["auth_token"], "fixture-token");
            assert_eq!(init["params"]["slot_id"], "worker-23");
            write_frame(&mut socket, br#"{"jsonrpc":"2.0","id":1,"result":{}}"#)
                .await
                .unwrap();
            let request: Value = serde_json::from_slice(&read_frame(&mut socket).await.unwrap()).unwrap();
            let result = if round == 0 {
                assert_eq!(request["method"], "tools/list");
                json!({"tools": [{"name": "team_task_list", "description": "Filtered task list",
                    "input_schema": {"type": "object"}}]})
            } else {
                assert_eq!(request["method"], "tools/call");
                assert_eq!(request["params"]["name"], "team_task_list");
                if round == 1 {
                    json!({"content": [{"type": "text", "text": "fixture-result"}]})
                } else {
                    json!({"isError": true, "content": [{"type": "text", "text": "private-sentinel"}],
                    "structuredContent": {"domainCode": "FIXTURE_ERROR"}})
                }
            };
            let response = json!({"jsonrpc": "2.0", "id": 2, "result": result});
            write_frame(&mut socket, &serde_json::to_vec(&response).unwrap())
                .await
                .unwrap();
        }
    });
    let (server_io, client_io) = tokio::io::duplex(8192);
    let server = tokio::spawn(async move {
        TeamStdioServer {
            port,
            token: "fixture-token".into(),
            slot_id: "worker-23".into(),
        }
        .serve(server_io)
        .await
        .unwrap()
        .waiting()
        .await
        .unwrap();
    });
    let mut wire = BufReader::new(client_io);
    send(
        &mut wire,
        json!({"jsonrpc": "2.0", "id": 10, "method": "initialize", "params": {
            "protocolVersion": "2025-11-25", "capabilities": {},
            "clientInfo": {"name": "host23-fixture", "version": "1"}
        }}),
    )
    .await;
    let initialized = reply(&mut wire).await;
    assert_eq!(initialized["id"], 10);
    assert!(initialized["result"]["serverInfo"].is_object());
    send(
        &mut wire,
        json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
    )
    .await;
    send(&mut wire, json!({"jsonrpc": "2.0", "id": 11, "method": "tools/list"})).await;
    let listed = reply(&mut wire).await;
    assert_eq!(listed["result"]["tools"].as_array().unwrap().len(), 1);
    assert_eq!(listed["result"]["tools"][0]["name"], "team_task_list");
    assert_eq!(listed["result"]["tools"][0]["inputSchema"], json!({"type": "object"}));
    for id in [12, 13] {
        send(
            &mut wire,
            json!({"jsonrpc": "2.0", "id": id, "method": "tools/call",
            "params": {"name": "team_task_list", "arguments": {}}}),
        )
        .await;
        let called = reply(&mut wire).await;
        assert_eq!(called["id"], id);
        if id == 12 {
            assert_eq!(
                called["result"]["content"],
                json!([{"type": "text", "text": "fixture-result"}])
            );
        } else {
            assert_eq!(called["result"]["isError"], true);
            assert_eq!(called["result"]["structuredContent"]["code"], "MCP_TOOL_REMOTE_ERROR");
            assert_eq!(called["result"]["structuredContent"]["domainCode"], "FIXTURE_ERROR");
            assert!(!called.to_string().contains("private-sentinel"));
        }
    }
    tcp.await.unwrap();
    drop(wire);
    server.await.unwrap();
}
