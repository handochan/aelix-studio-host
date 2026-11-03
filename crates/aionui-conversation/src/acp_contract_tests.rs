//! Host contracts with synthetic ACP frames and the real SQLite repository.
use std::sync::Arc;

use aionui_ai_agent::protocol::events::{AgentStreamEvent, FinishEventData};
use aionui_ai_agent::{AcpError, AgentSendError};
use aionui_api_types::AgentErrorCode;
use aionui_common::{AgentType, ApiError};
use aionui_db::{IConversationRepository, SqliteConversationRepository, init_database_memory};
use aionui_realtime::BroadcastEventBus;
use serde_json::{Value, json};
use tokio::sync::broadcast;

use crate::convert::{row_to_message_response, row_to_message_response_compact};
use crate::error::ConversationError;
use crate::runtime_state::RuntimeLifecycleState;
use crate::stream_relay::{RelayOutcome, RelayTerminal, StreamRelay, TurnAttemptSummary};
use crate::turn_recovery_policy::{TurnRecoveryDecision, TurnRecoveryPolicy};

async fn repository() -> Arc<SqliteConversationRepository> {
    let db = init_database_memory().await.unwrap();
    sqlx::query("INSERT INTO users (id,user_type,username,password_hash,status,session_generation,created_at,updated_at) VALUES ('u','local','u','hash','active',0,1,1)")
        .execute(db.pool()).await.unwrap();
    sqlx::query("INSERT INTO conversations (id,user_id,name,type,extra,status,pinned,created_at,updated_at) VALUES ('c','u','Test','acp','{}','pending',0,1,1)")
        .execute(db.pool()).await.unwrap();
    Arc::new(SqliteConversationRepository::new(db.pool().clone()))
}

fn tool(update: Value, meta: Option<Value>) -> AgentStreamEvent {
    let mut data = json!({"session_id":"s", "update":update});
    if let Some(meta) = meta {
        data["_meta"] = meta;
    }
    serde_json::from_value(json!({"type":"acp_tool_call", "data":data})).unwrap()
}

#[tokio::test]
async fn acp_meta_merge_patch_and_reload_preserve_live_values_and_terminal_status() {
    let repo = repository().await;
    let bus = Arc::new(BroadcastEventBus::new(64));
    let mut websocket = bus.subscribe();
    let (tx, rx) = broadcast::channel(64);
    let relay = StreamRelay::new("c".into(), "msg".into(), "turn".into(), "u".into(), repo.clone(), bus);
    let final_children =
        json!([{"id":"child", "profile":"reviewer", "state":"done", "currentTool":"read", "elapsedMs":2300}]);
    for event in [
        tool(
            json!({"sessionUpdate":"tool_call", "tool_call_id":"parent", "status":"in_progress", "title":"agent", "rawInput":{"profile":"reviewer"}}),
            Some(json!({"keepThis":true,"removeThis":1,"nested":{"keep":1,"remove":2}})),
        ),
        tool(
            json!({"sessionUpdate":"tool_call_update", "tool_call_id":"parent", "status":"completed", "rawOutput":{"result":"x".repeat(70000)}}),
            Some(json!({"aelix/subagents":{"version":1,"children":[{"id":"child","state":"running"}]}})),
        ),
        // A late child event has _meta and no status, after the call completed.
        tool(
            json!({"sessionUpdate":"tool_call_update", "tool_call_id":"parent"}),
            Some(
                json!({"removeThis":null,"nested":{"remove":null,"added":3},"aelix/subagents":{"version":1,"children":final_children}}),
            ),
        ),
        // An omitted _meta on a later frame must preserve the stored metadata.
        tool(
            json!({"sessionUpdate":"tool_call_update", "tool_call_id":"parent", "title":"agent"}),
            None,
        ),
    ] {
        tx.send(event).unwrap();
    }
    tx.send(AgentStreamEvent::Finish(FinishEventData::default())).unwrap();
    relay.consume(rx).await;
    let row = repo.get_message("u", "c", "parent").await.unwrap().unwrap();
    assert_eq!(row.status.as_deref(), Some("finish"));
    let full = row_to_message_response(row.clone()).unwrap();
    assert_eq!(full.content["update"]["status"], "completed");
    assert_eq!(full.content["update"]["raw_input"]["profile"], "reviewer");
    let expected = json!({"keepThis":true,"nested":{"keep":1,"added":3},"aelix/subagents":{"version":1,"children":final_children}});
    assert_eq!(full.content["_meta"], expected);
    let preview = row_to_message_response_compact(row.clone()).unwrap();
    assert!(preview.content.get("_compact").is_some());
    assert_eq!(preview.content["_meta"], expected);
    let mut saw_late_meta = false;
    while let Ok(message) = websocket.try_recv() {
        if message.data.pointer("/data/_meta/aelix~1subagents/children") == Some(&final_children) {
            saw_late_meta = true;
        }
    }
    assert!(saw_late_meta, "latest child values must be delivered live too");
    // Recreate the read path repeatedly; no replay or execution is involved.
    for _ in 0..3 {
        let row = repo.get_message("u", "c", "parent").await.unwrap().unwrap();
        assert_eq!(row_to_message_response(row).unwrap().content["_meta"], expected);
    }
}

#[test]
fn compact_history_does_not_truncate_opaque_meta() {
    let meta = json!({"vendor/opaque":{"longValue":"z".repeat(70000)}});
    let row = aionui_db::models::MessageRow {
        id: "parent".into(),
        conversation_id: "c".into(),
        msg_id: None,
        r#type: "acp_tool_call".into(),
        content: json!({"_meta":meta,"update":{"rawOutput":"x".repeat(70000)}}).to_string(),
        position: Some("left".into()),
        status: Some("finish".into()),
        hidden: false,
        created_at: 1,
        backend_turn_id: None,
    };
    let response = row_to_message_response_compact(row).unwrap();
    assert_eq!(response.content["_meta"], meta);
    assert!(response.content["update"]["rawOutput"].as_str().unwrap().len() < 5000);
}

fn errors() -> Vec<(i32, Value, AgentErrorCode, &'static str)> {
    vec![
        (
            -33001,
            json!({"kind":"session_locked","sessionId":"s","lockedBy":"native","holder":null,"retryable":false}),
            AgentErrorCode::UserAgentSessionLocked,
            "USER_AGENT_SESSION_LOCKED",
        ),
        (
            -33002,
            json!({"kind":"turn_failed_after_dispatch","retryable":false,"cause":{"kind":"backend_error","command":"prompt"}}),
            AgentErrorCode::UserAgentTurnFailedAfterDispatch,
            "USER_AGENT_TURN_FAILED_AFTER_DISPATCH",
        ),
        (
            -32000,
            json!({"limit":16}),
            AgentErrorCode::UserAgentBusy,
            "USER_AGENT_BUSY",
        ),
        (
            -32004,
            json!({"kind":"restrictions_unenforceable","methods":["tools/set_active"]}),
            AgentErrorCode::UserAgentOperationRefused,
            "USER_AGENT_OPERATION_REFUSED",
        ),
        (
            -32004,
            json!({"kind":"profile_unavailable","profile":"reviewer"}),
            AgentErrorCode::UserAgentOperationRefused,
            "USER_AGENT_OPERATION_REFUSED",
        ),
        (
            -32004,
            json!({"kind":"extension_owned_by_launch","source":"fixture:setup"}),
            AgentErrorCode::UserAgentOperationRefused,
            "USER_AGENT_OPERATION_REFUSED",
        ),
    ]
}

#[tokio::test]
async fn structured_acp_errors_survive_http_stream_db_and_never_auto_replay() {
    for (protocol_code, data, code, locale_code) in errors() {
        let acp = || AcpError::OtherProtocolError {
            code: protocol_code,
            message: "private diagnostic must not leak".into(),
            data: Some(data.clone()),
        };
        let domain = ConversationError::Acp(acp());
        // Domain conversion must retain the structured ACP error as well.
        let stream = AgentSendError::from_agent_error(domain.to_agent_error()).into_stream_error();
        assert_eq!(stream.code, Some(code));
        assert_eq!(stream.protocol_code, Some(protocol_code));
        assert_eq!(stream.data.as_ref(), Some(&data));
        assert_eq!(stream.retryable, Some(false));
        assert!(!stream.message.contains("private diagnostic"));
        let http = ApiError::from(ConversationError::Acp(acp()));
        assert_eq!(http.error_code(), locale_code);
        assert_eq!(http.error_details().unwrap()["data"], data);
        assert_eq!(http.error_details().unwrap()["protocol_code"], protocol_code);
        let repo = repository().await;
        let bus = Arc::new(BroadcastEventBus::new(64));
        let mut websocket = bus.subscribe();
        let (tx, rx) = broadcast::channel(64);
        let relay = StreamRelay::new("c".into(), "msg".into(), "turn".into(), "u".into(), repo.clone(), bus);
        tx.send(AgentStreamEvent::Error(stream.clone())).unwrap();
        let outcome = relay.consume(rx).await;
        assert_eq!(
            TurnRecoveryPolicy::decide(AgentType::Acp, None, &outcome, RuntimeLifecycleState::Active, false),
            TurnRecoveryDecision::None
        );
        let mut saw_error = false;
        while let Ok(message) = websocket.try_recv() {
            if message.data["type"] == "error" {
                assert_eq!(message.data["data"]["data"], data);
                assert_eq!(message.data["data"]["code"], locale_code);
                saw_error = true;
            }
        }
        assert!(saw_error);
        let rows = repo
            .list_messages_page(
                "u",
                "c",
                &aionui_db::MessagePageParams {
                    limit: 100,
                    direction: aionui_db::MessagePageDirection::InitialLatest,
                },
            )
            .await
            .unwrap();
        let error_row = rows.items.iter().find(|r| r.r#type == "tips").unwrap();
        let persisted: Value = serde_json::from_str(&error_row.content).unwrap();
        assert_eq!(persisted["error"]["data"], data);
        // Empty-output errors must also stay out of the automatic replay path.
        let outcome = RelayOutcome {
            terminal: RelayTerminal::Error {
                code: Some(code),
                retryable: Some(false),
            },
            attempt: TurnAttemptSummary::default(),
            ..RelayOutcome::default()
        };
        assert_eq!(
            TurnRecoveryPolicy::decide(AgentType::Acp, None, &outcome, RuntimeLifecycleState::Active, false),
            TurnRecoveryDecision::None
        );
    }
}
