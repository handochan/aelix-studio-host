use aionui_common::TimestampMs;
use serde::{Deserialize, Serialize};

/// Row mapping for the `acp_session` table.
///
/// Stores ACP agent session state for suspend/resume across app restarts.
/// Primary key is `conversation_id` (one session per conversation).
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct AcpSessionRow {
    pub conversation_id: String,
    pub agent_source: String,
    pub agent_id: String,
    pub session_id: Option<String>,
    pub session_status: String,
    /// JSON object: serialized session configuration.
    pub session_config: String,
    pub last_active_at: Option<TimestampMs>,
    pub suspended_at: Option<TimestampMs>,
}

impl AcpSessionRow {
    /// A guarded fork stays published after its first durable ACP binding.
    pub fn has_published_fork(&self) -> bool {
        self.session_id.as_ref().is_some_and(|id| !id.is_empty())
            || serde_json::from_str::<serde_json::Value>(&self.session_config)
                .ok()
                .and_then(|value| value.get("fork_published").and_then(serde_json::Value::as_u64))
                == Some(1)
    }
}
