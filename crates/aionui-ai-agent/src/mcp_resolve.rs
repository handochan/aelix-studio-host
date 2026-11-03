//! Neutral MCP server resolution (Wave 0c E) — the SINGLE source of truth for
//! turning a conversation's configured MCP servers into the SDK-free
//! [`SessionMcpServer`] shape the clean-slate session stack carries in
//! `SessionConfig.init.mcp_servers`.
//!
//! The legacy per-backend resolvers (`factory::acp::load_user_mcp_servers` →
//! `Vec<agent_client_protocol::McpServer>`, `factory::aionrs::load_user_mcp_servers`
//! → `HashMap<String, McpServerConfig>`) emit SDK/engine-specific types. This
//! module emits the NEUTRAL `aionui_api_types::SessionMcpServer` so the app
//! boundary (`aionui-app`) can convert it once into the crate-local
//! `aionui_session::McpServerSpec`, and each backend serializes that into its own
//! wire shape. Same row-walking + selection + stdio-launch-resolution logic as
//! the legacy ACP path, but vendor-neutral.

use std::sync::Arc;

use aionui_api_types::{SessionMcpServer, SessionMcpTransport, TEAM_MCP_SERVER_NAME};
use aionui_db::IMcpServerRepository;
use aionui_db::models::McpServerRow;
use aionui_realtime::EventBroadcaster;

use tracing::{info, warn};

/// Resolve a conversation's user-configured MCP servers into neutral
/// [`SessionMcpServer`]s. `selected_ids = Some` → that frozen snapshot defines the
/// session (injected regardless of the row's global `enabled` flag); `None` → all
/// enabled rows. Builtin rows are excluded (guide/team MCP are folded separately
/// by the caller). Stdio launch commands are RESOLVED here (e.g. `npx` → the
/// bundled-node absolute path) so the spec that reaches `open_session` is final —
/// the Wave 0c contract that `McpTransport::Stdio.command` is pre-resolved.
///
/// Best-effort: a repo error, a capability-unsupported transport, or a malformed
/// `transport_config` row is warn-logged and skipped, never fatal. `broadcaster`
/// emits a user-scoped, non-terminal conversation advisory on resolution failure.
pub async fn resolve_session_mcp_servers(
    repo: &dyn IMcpServerRepository,
    user_id: &str,
    selected_ids: Option<&[String]>,
    conversation_id: &str,
    broadcaster: Arc<dyn EventBroadcaster>,
) -> Vec<SessionMcpServer> {
    let rows_result = match selected_ids {
        Some(ids) => repo.list_by_ids_any(user_id, ids).await,
        None => repo.list(user_id).await,
    };
    let rows = match rows_result {
        Ok(r) => r,
        Err(err) => {
            report_mcp_unavailable(
                broadcaster.as_ref(),
                user_id,
                conversation_id,
                "",
                "Configured MCP servers",
                "configuration_unavailable",
            )
            .await;
            warn!(conversation_id, error = %err, "mcp_resolve: list() failed; skipping injection");
            return Vec::new();
        }
    };

    let mut servers = Vec::with_capacity(rows.len());
    for row in rows {
        let selected = selected_ids
            .map(|ids| ids.iter().any(|id| id == &row.id))
            .unwrap_or(row.enabled);
        // `aionui-team` is a reserved wire-level name: the team coordination MCP
        // must win, so a user row that collides with it is skipped (never
        // injected), regardless of selection state.
        if !selected || row.builtin || row.name == TEAM_MCP_SERVER_NAME {
            continue;
        }
        match row_to_session_mcp_server(&row).await {
            Ok(server) => servers.push(server),
            Err(err) => {
                report_mcp_unavailable(broadcaster.as_ref(), user_id, conversation_id, &row.id, &row.name, &err).await;
                warn!(
                    conversation_id,
                    server_id = %row.id,
                    server_name = %row.name,
                    reason = "MCP_SERVER_UNAVAILABLE",
                    "mcp_resolve: failed to convert row; skipping"
                );
            }
        }
    }

    if !servers.is_empty() {
        info!(
            conversation_id,
            count = servers.len(),
            "mcp_resolve: resolved user MCP servers"
        );
    }
    servers
}

/// Emit an existing warning-tip shape, which the conversation UI renders without
/// ending a turn. Never overwrite connection-test state or echo commands/tokens.
pub(crate) async fn report_mcp_unavailable(
    bus: &dyn EventBroadcaster,
    user_id: &str,
    conversation_id: &str,
    server_id: &str,
    server_name: &str,
    error: &str,
) {
    let reason = if error.contains("checksum mismatch") {
        "Node archive checksum verification failed; restore trusted Node resources or correct the mirror."
    } else if error.contains("transport_config") {
        "The stored MCP configuration is invalid; correct its transport configuration."
    } else if error.contains("configuration_unavailable") {
        "MCP settings could not be loaded; check the host configuration storage."
    } else if error.contains("OUTBOUND_CONFIG_INVALID") {
        "The host mirror configuration is invalid; check the host mirror settings."
    } else if error.contains("AELIX_STUDIO_HOST_NODE_MIRROR") {
        "Node is unavailable; provide local/bundled Node or configure AELIX_STUDIO_HOST_NODE_MIRROR."
    } else if error.contains("OUTBOUND_DISABLED") {
        "Implicit package execution is disabled; configure the host registry mirror."
    } else if error.contains("unsupported") {
        "The agent does not support this MCP transport."
    } else {
        "The configured MCP command or transport is unavailable; check its configuration and local runtime."
    };
    warn!(conversation_id, server_id, reason, "MCP server could not be attached");
    let notice = aionui_api_types::McpUnavailableNotice {
        user_id: user_id.into(),
        conversation_id: conversation_id.into(),
        msg_id: aionui_common::generate_prefixed_id("mcp-notice"),
        turn_id: String::new(),
        kind: "tips",
        hidden: false,
        data: aionui_api_types::McpUnavailableTip {
            content: format!("MCP server '{}': {reason}", server_name),
            severity: "warning",
            code: "MCP_SERVER_UNAVAILABLE",
            params: serde_json::json!({"server_id":server_id}),
        },
    };
    bus.broadcast(aionui_api_types::WebSocketMessage::new(
        "message.stream",
        serde_json::to_value(notice).expect("notice serialization"),
    ));
}

/// Parse one `McpServerRow` into a neutral `SessionMcpServer`, resolving the stdio
/// launch command. Mirrors `factory::acp::row_to_sdk_mcp_server` but emits the
/// neutral type. Returns an error string when `transport_config` is malformed.
///
/// This is the SINGLE shared row→neutral conversion: `aionui-conversation` (team
/// snapshot refresh) and the session stack reuse it so the stdio command is
/// always normalized exactly once, the same way the direct claude/codex path
/// consumes it.
pub async fn row_to_session_mcp_server(row: &McpServerRow) -> Result<SessionMcpServer, String> {
    let value: serde_json::Value =
        serde_json::from_str(&row.transport_config).map_err(|e| format!("invalid transport_config JSON: {e}"))?;

    let transport = match row.transport_type.as_str() {
        "stdio" => {
            let command = value
                .get("command")
                .and_then(|v| v.as_str())
                .ok_or_else(|| "stdio: missing command".to_owned())?;
            let args: Vec<String> = value
                .get("args")
                .and_then(|v| v.as_array())
                .map(|arr| arr.iter().filter_map(|v| v.as_str().map(String::from)).collect())
                .unwrap_or_default();
            let mut env: std::collections::HashMap<String, String> = value
                .get("env")
                .and_then(|v| v.as_object())
                .map(|obj| {
                    obj.iter()
                        .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_owned())))
                        .collect()
                })
                .unwrap_or_default();

            // Resolve the launch command (npx/bun → bundled path) + fold in the
            // runtime-provided args prefix + env, exactly like the legacy
            // `ensure_stdio_launch`. The resolved form is what the agent spawns.
            let resolved = aionui_runtime::ensure_user_runtime_command_with_args(command, &args, None)
                .await
                .map_err(|e| e.to_string())?;
            let mut final_args: Vec<String> = resolved
                .args_prefix
                .iter()
                .map(|a| a.to_string_lossy().into_owned())
                .collect();
            final_args.extend(args);
            for (k, v) in resolved.env {
                env.insert(k.to_string_lossy().into_owned(), v.to_string_lossy().into_owned());
            }
            SessionMcpTransport::Stdio {
                command: resolved.program.to_string_lossy().into_owned(),
                args: final_args,
                env,
            }
        }
        "http" | "streamable_http" => {
            let url = value
                .get("url")
                .and_then(|v| v.as_str())
                .ok_or_else(|| "http: missing url".to_owned())?
                .to_owned();
            SessionMcpTransport::StreamableHttp {
                url,
                headers: parse_headers(value.get("headers")),
            }
        }
        "sse" => {
            let url = value
                .get("url")
                .and_then(|v| v.as_str())
                .ok_or_else(|| "sse: missing url".to_owned())?
                .to_owned();
            SessionMcpTransport::Sse {
                url,
                headers: parse_headers(value.get("headers")),
            }
        }
        other => return Err(format!("unknown transport type: {other}")),
    };

    Ok(SessionMcpServer {
        id: row.id.clone(),
        name: row.name.clone(),
        transport,
    })
}

/// Parse a JSON headers object into a `HashMap` (string values only).
fn parse_headers(value: Option<&serde_json::Value>) -> std::collections::HashMap<String, String> {
    value
        .and_then(|v| v.as_object())
        .map(|obj| {
            obj.iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_owned())))
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use aionui_db::models::McpServerRow;

    const TEST_USER_ID: &str = "user-1";

    struct MockRepo {
        rows: Vec<McpServerRow>,
        fail: bool,
    }

    #[async_trait::async_trait]
    impl IMcpServerRepository for MockRepo {
        async fn list(&self, user_id: &str) -> Result<Vec<McpServerRow>, aionui_db::DbError> {
            if self.fail {
                return Err(aionui_db::DbError::Init("boom".into()));
            }
            Ok(self.rows.iter().filter(|row| row.user_id == user_id).cloned().collect())
        }
        async fn find_by_id(&self, user_id: &str, id: &str) -> Result<Option<McpServerRow>, aionui_db::DbError> {
            Ok(self
                .rows
                .iter()
                .find(|row| row.user_id == user_id && row.id == id)
                .cloned())
        }
        async fn find_by_name(&self, user_id: &str, name: &str) -> Result<Option<McpServerRow>, aionui_db::DbError> {
            Ok(self
                .rows
                .iter()
                .find(|row| row.user_id == user_id && row.name == name)
                .cloned())
        }
        async fn create(
            &self,
            _params: aionui_db::CreateMcpServerParams<'_>,
        ) -> Result<McpServerRow, aionui_db::DbError> {
            unimplemented!("not needed")
        }
        async fn update(
            &self,
            _user_id: &str,
            _id: &str,
            _params: aionui_db::UpdateMcpServerParams<'_>,
        ) -> Result<McpServerRow, aionui_db::DbError> {
            unimplemented!("not needed")
        }
        async fn delete(&self, _user_id: &str, _id: &str) -> Result<(), aionui_db::DbError> {
            unimplemented!("not needed")
        }
        async fn batch_upsert(
            &self,
            _user_id: &str,
            _servers: &[aionui_db::CreateMcpServerParams<'_>],
        ) -> Result<Vec<McpServerRow>, aionui_db::DbError> {
            unimplemented!("not needed")
        }
        async fn update_status(
            &self,
            _user_id: &str,
            _id: &str,
            _status: &str,
            _last_connected: Option<aionui_common::TimestampMs>,
        ) -> Result<(), aionui_db::DbError> {
            unimplemented!("not needed")
        }
        async fn update_tools(
            &self,
            _user_id: &str,
            _id: &str,
            _tools: Option<&str>,
        ) -> Result<(), aionui_db::DbError> {
            unimplemented!("not needed")
        }
    }

    fn make_row(name: &str, enabled: bool) -> McpServerRow {
        McpServerRow {
            id: format!("mcp_{name}"),
            user_id: TEST_USER_ID.to_owned(),
            name: name.to_owned(),
            description: None,
            enabled,
            transport_type: "http".into(),
            transport_config: r#"{"url":"http://127.0.0.1:9999/mcp"}"#.into(),
            tools: None,
            last_test_status: "disconnected".into(),
            last_connected: None,
            original_json: None,
            builtin: false,
            deleted_at: None,
            created_at: 0,
            updated_at: 0,
        }
    }

    fn test_broadcaster() -> Arc<dyn EventBroadcaster> {
        Arc::new(aionui_realtime::BroadcastEventBus::new(16))
    }

    #[tokio::test]
    async fn resolve_skips_reserved_team_mcp_name() {
        let repo = MockRepo {
            rows: vec![make_row("docs", true), make_row(TEAM_MCP_SERVER_NAME, true)],
            fail: false,
        };
        let servers = resolve_session_mcp_servers(&repo, TEST_USER_ID, None, "conv-1", test_broadcaster()).await;
        assert_eq!(servers.len(), 1);
        assert_eq!(servers[0].name, "docs");
    }

    #[tokio::test]
    async fn resolve_skips_disabled_and_builtin_rows() {
        let repo = MockRepo {
            rows: vec![
                make_row("docs", true),
                make_row("off", false),
                McpServerRow {
                    id: "mcp_builtin".into(),
                    user_id: TEST_USER_ID.to_owned(),
                    name: "chrome-devtools".into(),
                    description: None,
                    enabled: true,
                    transport_type: "stdio".into(),
                    transport_config: r#"{"command":"/bin/true","args":[],"env":{}}"#.into(),
                    tools: None,
                    last_test_status: "disconnected".into(),
                    last_connected: None,
                    original_json: None,
                    builtin: true,
                    deleted_at: None,
                    created_at: 0,
                    updated_at: 0,
                },
            ],
            fail: false,
        };
        let servers = resolve_session_mcp_servers(&repo, TEST_USER_ID, None, "conv-1", test_broadcaster()).await;
        assert_eq!(servers.len(), 1);
        assert_eq!(servers[0].name, "docs");
    }
    #[tokio::test]
    async fn failed_mcp_resolution_notifies_owner_without_overwriting_test_status() {
        use aionui_db::{IUserRepository, SqliteMcpServerRepository, SqliteUserRepository};
        let db = aionui_db::init_database_memory().await.unwrap();
        let users = SqliteUserRepository::new(db.pool().clone());
        let owner = users.create_user("mcp-owner", "hash").await.unwrap().id;
        let other = users.create_user("mcp-other", "hash").await.unwrap().id;
        let repo = SqliteMcpServerRepository::new(db.pool().clone());
        let row = repo
            .create(aionui_db::CreateMcpServerParams {
                user_id: &owner,
                name: "missing-tool",
                description: None,
                enabled: true,
                transport_type: "stdio",
                transport_config: r#"{"command":"/missing/secret-sentinel","env":{"TOKEN":"secret-sentinel"}}"#,
                tools: None,
                original_json: None,
                builtin: false,
            })
            .await
            .unwrap();
        repo.update_status(&owner, &row.id, "connected", None).await.unwrap();
        let bus = Arc::new(aionui_realtime::BroadcastEventBus::new(16));
        let mut events = bus.subscribe();
        assert!(
            resolve_session_mcp_servers(&repo, &owner, None, "conv-visible", bus.clone())
                .await
                .is_empty()
        );
        let notice = events.try_recv().unwrap();
        assert_eq!(notice.name, "message.stream");
        assert_eq!(notice.data["user_id"], owner);
        assert_eq!(notice.data["conversation_id"], "conv-visible");
        assert_eq!(notice.data["type"], "tips");
        assert_eq!(notice.data["data"]["type"], "warning");
        assert_eq!(notice.data["data"]["code"], "MCP_SERVER_UNAVAILABLE");
        assert!(
            notice.data["data"]["content"]
                .as_str()
                .unwrap()
                .contains("configured MCP command")
        );
        assert!(!notice.data.to_string().contains("secret-sentinel"));
        assert_eq!(
            repo.find_by_id(&owner, &row.id)
                .await
                .unwrap()
                .unwrap()
                .last_test_status,
            "connected"
        );
        assert!(
            resolve_session_mcp_servers(&repo, &other, None, "other-conv", bus)
                .await
                .is_empty()
        );
        assert!(events.try_recv().is_err());
    }
}
